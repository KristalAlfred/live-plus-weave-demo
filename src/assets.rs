use std::path::Path;

/// The pages ship inside the binary so the demo is one artefact. Naming each
/// file here is also what keeps a request from reaching anything else on disk
/// when `GREENROOM_ASSETS` points the gateway at a directory instead.
const EMBEDDED: &[(&str, &str, &str)] = &[
    (
        "base.css",
        "text/css; charset=utf-8",
        include_str!("../assets/base.css"),
    ),
    (
        "guest.html",
        "text/html; charset=utf-8",
        include_str!("../assets/guest.html"),
    ),
    (
        "guest.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/guest.js"),
    ),
    (
        "operator.html",
        "text/html; charset=utf-8",
        include_str!("../assets/operator.html"),
    ),
    (
        "operator.js",
        "text/javascript; charset=utf-8",
        include_str!("../assets/operator.js"),
    ),
];

pub struct Asset {
    pub content_type: &'static str,
    pub body: String,
}

pub async fn load(name: &str, from: Option<&Path>) -> Option<Asset> {
    let (_, content_type, embedded) = EMBEDDED.iter().find(|(file, _, _)| *file == name)?;
    let body = match from {
        Some(dir) => match tokio::fs::read_to_string(dir.join(name)).await {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%name, %error, "falling back to the embedded copy");
                (*embedded).to_string()
            }
        },
        None => (*embedded).to_string(),
    };
    Some(Asset {
        content_type,
        body,
    })
}
