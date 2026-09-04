use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use weave_core::NodeStatus;

use crate::app::App;
use crate::snapshot::NodeFacts;

/// Reconciles once every interval, and immediately when nudged. The webhook is
/// the fast path onto this loop, not a second way of doing the work: a lost
/// delivery costs one interval of latency and nothing else.
pub async fn run(app: Arc<App>) {
    loop {
        tick(&app).await;
        tokio::select! {
            () = tokio::time::sleep(app.cfg.reconcile) => {}
            () = app.nudge.notified() => {}
        }
    }
}

pub async fn tick(app: &App) {
    let now = crate::now();
    let seats = app.seats.lock().expect("seat store lock").list();
    let mut errors = Vec::new();

    let nodes = match app.weave.nodes().await {
        Ok(nodes) => Some(
            nodes
                .into_iter()
                .map(|node| (node.id, NodeFacts { status: node.status }))
                .collect::<BTreeMap<_, _>>(),
        ),
        Err(error) => {
            errors.push(format!("{error:#}"));
            None
        }
    };

    let declared = match app.weave.streams().await {
        Ok(streams) => Some(
            streams
                .into_iter()
                .map(|stream| stream.name)
                .collect::<BTreeSet<_>>(),
        ),
        Err(error) => {
            errors.push(format!("{error:#}"));
            None
        }
    };

    let streams = match app.weave.status().await {
        Ok(rollup) => rollup
            .streams
            .into_iter()
            .map(|stream| (stream.name.clone(), stream))
            .collect::<BTreeMap<_, _>>(),
        Err(error) => {
            errors.push(format!("{error:#}"));
            BTreeMap::new()
        }
    };

    let sources = match &app.live {
        Some(live) => match live.sources().await {
            Ok(sources) => sources
                .into_iter()
                .filter_map(|source| Some((source.seat()?.to_string(), source)))
                .collect(),
            Err(error) => {
                errors.push(format!("{error:#}"));
                BTreeMap::new()
            }
        },
        None => BTreeMap::new(),
    };

    // Nothing is changed unless weave answered both questions. An unreachable
    // northbound reads as an empty stream list, and acting on that would
    // withdraw every guest's stream.
    if let (Some(nodes), Some(declared)) = (&nodes, &declared) {
        for seat in &seats {
            let online = matches!(
                nodes.get(&seat.seat).map(|node| node.status),
                Some(NodeStatus::Ready | NodeStatus::Degraded)
            );
            let exists = declared.contains(&seat.seat);
            // A guest whose page is merely reloading keeps the stream: the node
            // stays known to weave until its TTL, and the seat until revoked.
            let unwanted = seat.revoked || seat.expired(now);

            if online && !unwanted && !exists {
                let stream = app.weave.stream_for(
                    &seat.seat,
                    &app.cfg.target_node,
                    app.cfg.target_network.as_deref(),
                    app.cfg.target_latency_ms,
                );
                match app.weave.declare_stream(&stream).await {
                    Ok(()) => tracing::info!(
                        seat = %seat.seat,
                        target = %app.cfg.target_node,
                        "declared a stream for the guest's camera"
                    ),
                    Err(error) => errors.push(format!("{error:#}")),
                }
            }

            if unwanted && exists {
                match app.weave.delete_stream(&seat.seat).await {
                    Ok(_) => tracing::info!(seat = %seat.seat, "withdrew the guest's stream"),
                    Err(error) => errors.push(format!("{error:#}")),
                }
            }

            if seat.joined_at.is_none() && online {
                let joined = app
                    .seats
                    .lock()
                    .expect("seat store lock")
                    .update(&seat.seat, |seat| {
                        seat.joined_at = Some(now);
                        true
                    });
                if let Err(error) = joined {
                    errors.push(format!("{error:#}"));
                }
            }

            // A revoked seat is only forgotten once its stream is gone, so a
            // failed delete is retried instead of leaking an orphan.
            if seat.revoked
                && !exists
                && let Err(error) = app.seats.lock().expect("seat store lock").remove(&seat.seat)
            {
                errors.push(format!("{error:#}"));
            }
        }
    }

    // A hop status is only true of a node that is still heartbeating. Keeping
    // one past that would report a guest who has left as still sending.
    let reporting: BTreeSet<String> = seats
        .into_iter()
        .map(|seat| seat.seat)
        .filter(|seat| {
            nodes.as_ref().is_some_and(|nodes| {
                matches!(
                    nodes.get(seat).map(|node| node.status),
                    Some(NodeStatus::Ready | NodeStatus::Degraded)
                )
            })
        })
        .collect();

    let mut snapshot = app.snapshot.write().expect("snapshot lock");
    snapshot.reconciled_at = crate::rfc3339(now);
    snapshot.nodes = nodes.unwrap_or_default();
    snapshot.streams = streams;
    snapshot.sources = sources;
    snapshot.errors = errors;
    snapshot.reported.retain(|seat, _| reporting.contains(seat));
}
