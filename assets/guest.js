"use strict";

const HEARTBEAT_MS = 5000;
const POLL_MS = 2000;
const STALL_POLLS = 3;
const RETRY_MS = 5000;
const FAILED_AFTER_ATTEMPTS = 5;
const ICE_GATHER_MS = 2000;
const DEVICE_TRANSPORT = "device";

const token = location.pathname.split("/").pop();
const hops = new Map();
const timers = [];
const logLines = [];

let session = null;
let stream = null;
let registered = false;
let joined = false;
let fatal = null;

// --- gateway ---

// The gateway signs the southbound calls and pins the node id, so this page
// holds no weave token and never talks to the control plane itself.
async function gateway(method, path, body) {
  return fetch(`/api/session/${encodeURIComponent(token)}${path}`, {
    method,
    headers: body === undefined ? {} : { "content-type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

async function errorText(response) {
  try {
    const body = await response.json();
    if (body && body.error) return body.error;
  } catch {}
  return `the gateway answered ${response.status}`;
}

// An unknown, expired or revoked invite ends the page wherever it is noticed,
// on the first load or on a later poll.
async function fetchSession(initial) {
  let response;
  try {
    response = await gateway("GET", "");
  } catch (error) {
    if (initial) terminal(error.message);
    else log(`session: ${error.message}`);
    return false;
  }
  if (!response.ok) {
    if (initial || response.status === 404 || response.status === 410) {
      terminal(await errorText(response));
    } else {
      log(`session: ${response.status}`);
    }
    return false;
  }
  session = await response.json();
  return true;
}

async function pollSession() {
  if (await fetchSession(false)) render();
}

async function register() {
  const response = await gateway("POST", "/register", { hop_status: hopStatuses() });
  if (!response.ok) throw new Error(`register: ${response.status} ${(await response.text()).trim()}`);
  registered = true;
  log(`registered as ${session.seat}`);
}

async function heartbeat() {
  if (fatal || !joined) return;
  try {
    if (!registered) {
      await register();
      return;
    }
    const response = await gateway("POST", "/heartbeat", { hop_status: hopStatuses() });
    if (response.status === 404) registered = false;
    else if (!response.ok) throw new Error(`heartbeat: ${response.status}`);
  } catch (error) {
    registered = false;
    log(error.message);
  } finally {
    render();
  }
}

async function pollDesired() {
  if (fatal || !joined || !registered) return;
  try {
    const response = await gateway("GET", "/desired");
    if (!response.ok) throw new Error(`desired: ${response.status}`);
    reconcile(await response.json());
  } catch (error) {
    log(error.message);
  }
  for (const hop of hops.values()) await hop.observe();
  render();
}

// Hops are matched by id. A hop whose spec changed is torn down and restarted;
// an unchanged one is left alone, whatever its state.
function reconcile(desired) {
  const wanted = new Map(desired.map(spec => [spec.id, spec]));
  for (const [id, hop] of hops) {
    const spec = wanted.get(id);
    if (!spec || JSON.stringify(spec) !== JSON.stringify(hop.spec)) {
      hop.stop();
      hops.delete(id);
    }
  }
  for (const [id, spec] of wanted) {
    if (hops.has(id)) continue;
    const hop = Hop.from(spec);
    hops.set(id, hop);
    hop.start();
  }
}

function hopStatuses() {
  return Array.from(hops.values(), hop => hop.status());
}

// --- byte progress ---

// Byte progress across polls is what separates a connected session that is
// carrying media from one that is silent; a counter that stops moving after
// having moved is a stall.
class Progress {
  constructor() {
    this.last = null;
    this.lastAt = 0;
    this.stale = 0;
    this.ever = false;
    this.advanced = false;
    this.rateMbps = 0;
  }

  observe(bytes) {
    const now = performance.now();
    if (bytes == null) return;
    if (this.last != null) {
      if (bytes > this.last) {
        this.ever = true;
        this.stale = 0;
        this.advanced = true;
        const seconds = (now - this.lastAt) / 1000;
        this.rateMbps = seconds > 0 ? ((bytes - this.last) * 8) / seconds / 1e6 : 0;
      } else {
        this.stale += 1;
        this.advanced = false;
        this.rateMbps = 0;
      }
    }
    this.last = bytes;
    this.lastAt = now;
  }

  condition(connected) {
    if (!connected) return "connecting";
    if (this.ever && this.stale >= STALL_POLLS) return "stalled";
    if (this.advanced) return "flowing";
    return "connected";
  }
}

// --- hops ---

function socketName(socket) {
  if (!socket) return "nothing";
  return socket.transport === DEVICE_TRANSPORT ? `${socket.role} device` : socket.transport;
}

class Hop {
  static from(spec) {
    const egress = spec.egresses[0];
    if (spec.ingress.transport === DEVICE_TRANSPORT && egress && egress.transport === "whip") {
      return new SenderHop(spec);
    }
    return new UnsupportedHop(spec);
  }

  constructor(spec) {
    this.spec = spec;
    this.pc = null;
    this.resource = null;
    this.error = null;
    this.transient = false;
    this.attempts = 0;
    this.retry = null;
    this.stopped = false;
    this.progress = new Progress();
    this.stats = null;
    this.connected = false;
  }

  async start() {
    try {
      await this.open();
      this.error = null;
      this.transient = false;
      this.attempts = 0;
    } catch (error) {
      this.attempts += 1;
      this.error = error.message;
      // The WHIP receiver allows one session at a time and answers 503 until the
      // previous one has finished tearing down, which is what a guest reloading
      // the page hits; that is a retry, not something to alarm them with.
      this.transient = error.status === 503 || this.progress.ever;
      log(`${this.spec.id}: ${error.message}`);
      this.close();
      if (!this.stopped) this.retry = setTimeout(() => this.start(), RETRY_MS);
    }
    render();
  }

  stop() {
    this.stopped = true;
    clearTimeout(this.retry);
    this.close();
  }

  close() {
    if (this.resource) {
      fetch(this.resource, { method: "DELETE" }).catch(() => {});
      this.resource = null;
    }
    if (this.pc) {
      this.pc.close();
      this.pc = null;
    }
    this.connected = false;
  }

  newPeerConnection() {
    const pc = new RTCPeerConnection();
    pc.onconnectionstatechange = () => {
      if (["failed", "disconnected", "closed"].includes(pc.connectionState) && this.pc === pc) {
        log(`${this.spec.id}: connection ${pc.connectionState}; reconnecting`);
        this.transient = true;
        this.close();
        if (!this.stopped) this.retry = setTimeout(() => this.start(), RETRY_MS);
        render();
      }
    };
    this.pc = pc;
    return pc;
  }

  // WHIP signalling is one exchange: POST the offer as SDP, read the answer,
  // remember the session resource from Location for teardown. A cross-origin
  // receiver may not expose Location, so the resource can stay unknown and
  // teardown then relies on the receiver's inactivity timeout.
  async signal(url) {
    const pc = this.pc;
    await pc.setLocalDescription(await pc.createOffer());
    await iceGathered(pc, ICE_GATHER_MS);
    const response = await fetch(url, {
      method: "POST",
      headers: { "content-type": "application/sdp" },
      body: pc.localDescription.sdp,
    });
    if (!response.ok) {
      const error = new Error(`${url} → ${response.status} ${(await response.text()).trim()}`);
      error.status = response.status;
      throw error;
    }
    const answer = await response.text();
    const location = response.headers.get("location");
    this.resource = location ? new URL(location, url).href : null;
    if (!this.resource) log(`${this.spec.id}: session created; Location not exposed to this origin, teardown relies on the server's timeout`);
    if (this.pc !== pc) return;
    await pc.setRemoteDescription({ type: "answer", sdp: answer });
  }

  nodeId() {
    return this.spec.node_id || (session ? session.seat : "");
  }

  state() {
    if (this.error) return this.attempts >= FAILED_AFTER_ATTEMPTS ? "failed" : "pending";
    return this.pc ? "provisioned" : "pending";
  }

  async observe() {
    if (!this.pc) {
      this.connected = false;
      this.stats = null;
      return;
    }
    this.connected = this.pc.connectionState === "connected";
    const report = await this.pc.getStats();
    const totals = { bytesSent: 0, bytesReceived: 0, packetsLost: 0, sentLost: 0, retransmittedSent: 0, retransmittedReceived: 0 };
    for (const entry of report.values()) {
      if (entry.type === "outbound-rtp") {
        totals.bytesSent += entry.bytesSent || 0;
        totals.retransmittedSent += entry.retransmittedPacketsSent || 0;
      } else if (entry.type === "remote-inbound-rtp") {
        totals.sentLost += entry.packetsLost || 0;
      } else if (entry.type === "inbound-rtp") {
        totals.bytesReceived += entry.bytesReceived || 0;
        totals.packetsLost += entry.packetsLost || 0;
        totals.retransmittedReceived += entry.retransmittedPacketsReceived || 0;
      }
    }
    this.progress.observe(this.sending ? totals.bytesSent : totals.bytesReceived);
    this.stats = {
      connections: this.connected ? 1 : 0,
      ingress_rate_mbps: this.sending ? 0 : this.progress.rateMbps,
      egress_rate_mbps: this.sending ? this.progress.rateMbps : 0,
      packets_sent_lost: totals.sentLost,
      packets_retransmitted: totals.retransmittedSent,
      packets_received_lost: totals.packetsLost,
      packets_received_retransmitted: totals.retransmittedReceived,
    };
  }

  status() {
    const status = {
      id: this.spec.id,
      node_id: this.nodeId(),
      state: this.state(),
      ingress: this.ingressCondition(),
      egress: this.egressCondition(),
    };
    if (this.stats) status.stats = this.stats;
    return status;
  }
}

// device (camera) → whip connect
class SenderHop extends Hop {
  constructor(spec) {
    super(spec);
    this.sending = true;
  }

  async open() {
    if (!stream) throw new Error("the camera is not open");
    const pc = this.newPeerConnection();
    for (const track of stream.getTracks()) {
      const transceiver = pc.addTransceiver(track, { direction: "sendonly" });
      if (track.kind === "video") preferH264(transceiver);
    }
    await this.signal(this.spec.egresses[0].url);
  }

  // The camera is this hop's ingress: producing while its tracks are live.
  ingressCondition() {
    const tracks = stream ? stream.getTracks() : [];
    if (!tracks.some(track => track.readyState === "live")) return "idle";
    return tracks.some(track => !track.muted) ? "flowing" : "connected";
  }

  egressCondition() {
    return this.progress.condition(this.connected);
  }
}

class UnsupportedHop extends Hop {
  async start() {
    this.error = `this page realises device→whip only, not ${socketName(this.spec.ingress)}→${socketName(this.spec.egresses[0])}`;
    log(`${this.spec.id}: ${this.error}`);
    render();
  }

  state() {
    return "failed";
  }

  ingressCondition() {
    return "idle";
  }

  egressCondition() {
    return "idle";
  }
}

// The studio's WHIP input decodes H.264 only, and an offer that carries just
// VP8/VP9 connects and then never flows, so refusing is clearer than sending
// into silence.
function preferH264(transceiver) {
  const capabilities = RTCRtpSender.getCapabilities("video");
  const codecs = (capabilities ? capabilities.codecs : []).filter(
    codec => codec.mimeType.toLowerCase() === "video/h264",
  );
  if (!codecs.length) throw new Error("this browser cannot send H.264, which the studio requires");
  if (transceiver.setCodecPreferences) transceiver.setCodecPreferences(codecs);
}

function iceGathered(pc, timeoutMs) {
  if (pc.iceGatheringState === "complete") return Promise.resolve();
  return new Promise(resolve => {
    const done = () => {
      pc.removeEventListener("icegatheringstatechange", check);
      resolve();
    };
    const check = () => {
      if (pc.iceGatheringState === "complete") done();
    };
    pc.addEventListener("icegatheringstatechange", check);
    setTimeout(done, timeoutMs);
  });
}

// --- devices ---

const previews = [document.getElementById("preview-join"), document.getElementById("preview-live")];
const cameraSelect = document.getElementById("camera");
const micSelect = document.getElementById("mic");
const joinButton = document.getElementById("join");
const joinError = document.getElementById("join-error");
const meterFill = document.getElementById("meter");

let audioContext = null;
let meterSource = null;
let meterFrame = 0;

async function openPreview() {
  if (!navigator.mediaDevices) {
    throw new Error("this page needs a secure context (https, or http on localhost) to use the camera");
  }
  const next = await navigator.mediaDevices.getUserMedia({
    video: cameraSelect.value ? { deviceId: { exact: cameraSelect.value } } : true,
    audio: micSelect.value ? { deviceId: { exact: micSelect.value } } : true,
  });
  if (!next.getVideoTracks().length || !next.getAudioTracks().length) {
    for (const track of next.getTracks()) track.stop();
    throw new Error("both a camera and a microphone are needed to join");
  }
  stopPreview();
  stream = next;
  for (const preview of previews) {
    preview.srcObject = stream;
    preview.play().catch(() => {});
  }
  startMeter(stream);
}

function stopPreview() {
  stopMeter();
  if (!stream) return;
  for (const track of stream.getTracks()) track.stop();
  stream = null;
  for (const preview of previews) preview.srcObject = null;
}

function startMeter(source) {
  stopMeter();
  if (!audioContext) {
    const Context = window.AudioContext || window.webkitAudioContext;
    if (!Context) return;
    audioContext = new Context();
  }
  audioContext.resume().catch(() => {});
  const analyser = audioContext.createAnalyser();
  analyser.fftSize = 1024;
  meterSource = audioContext.createMediaStreamSource(source);
  meterSource.connect(analyser);
  const samples = new Float32Array(analyser.fftSize);
  const tick = () => {
    analyser.getFloatTimeDomainData(samples);
    let sum = 0;
    for (const sample of samples) sum += sample * sample;
    const rms = Math.sqrt(sum / samples.length);
    meterFill.style.width = `${Math.min(100, rms * 400).toFixed(1)}%`;
    meterFrame = requestAnimationFrame(tick);
  };
  tick();
}

function stopMeter() {
  cancelAnimationFrame(meterFrame);
  meterFrame = 0;
  if (meterSource) {
    meterSource.disconnect();
    meterSource = null;
  }
  meterFill.style.width = "0";
}

// Labels arrive only once permission has been granted, so enumeration runs
// after the first getUserMedia.
async function listDevices() {
  const devices = await navigator.mediaDevices.enumerateDevices();
  const groups = [
    [cameraSelect, "videoinput", "Camera", stream.getVideoTracks()[0]],
    [micSelect, "audioinput", "Microphone", stream.getAudioTracks()[0]],
  ];
  for (const [select, kind, label, track] of groups) {
    const matching = devices.filter(device => device.kind === kind);
    select.replaceChildren();
    matching.forEach((device, index) => {
      const option = el("option", null, device.label || `${label} ${index + 1}`);
      option.value = device.deviceId;
      select.append(option);
    });
    const active = track ? track.getSettings().deviceId : null;
    if (active && matching.some(device => device.deviceId === active)) select.value = active;
  }
}

async function changeDevice() {
  joinButton.disabled = true;
  try {
    await openPreview();
    showJoinError(null);
    joinButton.disabled = false;
  } catch (error) {
    showJoinError(error.message);
  }
}

function showJoinError(message) {
  joinError.textContent = message ? `We couldn’t open your camera and microphone. ${message}` : "";
  joinError.hidden = !message;
}

// --- page ---

function log(line) {
  logLines.unshift(`${new Date().toLocaleTimeString()} ${line}`);
  logLines.length = Math.min(logLines.length, 40);
}

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text != null) node.textContent = text;
  return node;
}

const STEP_LABELS = {
  invited: "Invited",
  joined: "Joined",
  routed: "Routed",
  flowing: "Media flowing",
  "open-live": "In the studio",
};
const STEP_STATES = ["pending", "active", "done", "failed"];

function showScreen(name) {
  for (const screen of ["terminal", "join", "live", "ended"]) {
    document.getElementById(`screen-${screen}`).hidden = screen !== name;
  }
}

function teardown() {
  for (const timer of timers) clearInterval(timer);
  timers.length = 0;
  for (const hop of hops.values()) hop.stop();
  hops.clear();
  stopPreview();
  joined = false;
  registered = false;
}

function terminal(message) {
  teardown();
  fatal = message;
  document.getElementById("terminal-message").textContent = message;
  showScreen("terminal");
}

function sender() {
  for (const hop of hops.values()) if (hop instanceof SenderHop) return hop;
  return null;
}

function liveState() {
  const hop = sender();
  if (!hop) return "connecting";
  const egress = hop.egressCondition();
  if (egress === "flowing" || (hop.progress.ever && egress === "connected")) return "live";
  if (hop.progress.ever || hop.attempts > 0) return "reconnecting";
  return "connecting";
}

function render() {
  if (fatal || !joined) return;
  const state = liveState();
  const status = document.getElementById("status");
  status.textContent = state;
  status.className = state === "live" ? "pill live" : "pill wait";

  const hop = sender();
  const rate = hop && hop.stats ? hop.stats.egress_rate_mbps : 0;
  document.getElementById("bitrate").textContent = rate > 0 ? `${rate.toFixed(2)} Mb/s` : "";

  const failure = hop && hop.error && !hop.transient ? hop.error : null;
  document.getElementById("live-note").textContent =
    failure || "Keep this page open. Closing it ends your feed.";

  renderChain();
  renderDetail();
  document.getElementById("log").textContent = logLines.join("\n");
}

function renderChain() {
  const rail = document.getElementById("chain");
  rail.replaceChildren();
  const steps = session && Array.isArray(session.chain) ? session.chain : [];
  for (const step of steps) {
    const state = STEP_STATES.includes(step.state) ? step.state : "pending";
    const node = el("div", `step ${state}`);
    if (step.detail) node.title = step.detail;
    node.append(el("span", "dot"), el("span", "label", STEP_LABELS[step.step] || step.step));
    rail.append(node);
  }
}

function renderDetail() {
  const rows = [
    ["seat", session.seat],
    ["stage", session.stage],
    ["control plane", registered ? "registered" : "registering"],
    ["invite expires", session.expires_at],
  ];
  for (const step of session.chain || []) {
    if (step.detail) rows.push([step.step, `${step.state} · ${step.detail}`]);
  }
  for (const hop of hops.values()) {
    const status = hop.status();
    rows.push([status.id, `${status.state} · in ${status.ingress} · out ${status.egress}`]);
    const egress = hop.spec.egresses[0];
    if (egress && egress.url) rows.push(["whip", egress.url]);
    if (hop.error) rows.push(["last error", hop.error]);
  }
  const detail = document.getElementById("detail");
  detail.replaceChildren(...rows.map(([key, value]) => {
    const row = el("div", "kv");
    row.append(el("span", "k", key), el("span", "v", value == null ? "—" : String(value)));
    return row;
  }));
}

async function join() {
  joinButton.disabled = true;
  joined = true;
  document.getElementById("live-title").textContent =
    session.display_name ? `${session.display_name} · you’re on` : "You’re on";
  showScreen("live");
  render();
  await heartbeat();
  timers.push(
    setInterval(heartbeat, HEARTBEAT_MS),
    setInterval(pollDesired, POLL_MS),
    setInterval(pollSession, POLL_MS),
  );
}

function leave() {
  teardown();
  showScreen("ended");
}

async function start() {
  if (!(await fetchSession(true))) return;
  document.getElementById("greeting").textContent = session.display_name
    ? `Hi ${session.display_name}, you’re invited to join the broadcast`
    : "You’re invited to join the broadcast";
  showScreen("join");
  try {
    await openPreview();
    await listDevices();
    joinButton.disabled = false;
  } catch (error) {
    showJoinError(error.message);
    log(`camera: ${error.message}`);
  }
}

cameraSelect.addEventListener("change", changeDevice);
micSelect.addEventListener("change", changeDevice);
joinButton.addEventListener("click", join);
document.getElementById("leave").addEventListener("click", leave);
document.addEventListener("pointerdown", () => {
  if (audioContext) audioContext.resume().catch(() => {});
}, { once: true });

start();
