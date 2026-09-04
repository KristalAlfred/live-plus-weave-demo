// Drive the guest page without a human: create an invite through the gateway,
// open the join link in Chromium with a fake camera, press Join, and report
// how far the guest travelled.
//
//   node check/guest.mjs [--name Alice] [--gateway http://localhost:8090]
//                        [--stay] [--headed] [--timeout 60]
//
// --stay leaves the browser running once the guest is live, which is how the
// demo keeps a guest on air without a second machine.
import { chromium } from "playwright";

const args = parse(process.argv.slice(2));
const gateway = (args.gateway || process.env.GREENROOM_PUBLIC_URL || "http://localhost:8090")
  .replace(/\/+$/, "");
const name = args.name || "Alice";
const timeout = Number(args.timeout || 60) * 1000;

const invite = await post(`${gateway}/api/invites`, { name });
console.log(`invited ${invite.display_name} as ${invite.seat}`);
console.log(`  ${invite.join_url}`);

// Playwright's headless shell never answers getUserMedia for the fake devices,
// so the full Chromium build is what this launches.
const browser = await chromium.launch({
  channel: "chromium",
  headless: !args.headed,
  args: ["--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"],
});
const page = await browser.newPage();
page.on("console", message => {
  if (message.type() === "error") console.log(`  page error: ${message.text()}`);
});
await page.goto(invite.join_url, { waitUntil: "domcontentloaded" });

const join = page.locator("button.primary");
await join.waitFor({ state: "visible", timeout: 15000 });
await page.waitForFunction(() => !document.querySelector("button.primary").disabled, null, {
  timeout: 15000,
});
await join.click();
console.log("joined; watching the chain");

const deadline = Date.now() + timeout;
let last = "";
let reached = false;
while (Date.now() < deadline) {
  const seat = await seatOf(invite.seat);
  if (seat) {
    const line = seat.chain
      .map(step => `${step.step}:${step.state}`)
      .join(" ");
    if (line !== last) {
      last = line;
      console.log(`  ${seat.stage.padEnd(9)} ${line}`);
      for (const step of seat.chain) {
        if (step.detail) console.log(`      ${step.step}: ${step.detail}`);
      }
    }
    // The last step of `flowing` needs something to pull the gateway's SRT
    // output, which is open-live's activated production — an operator action,
    // not this script's. Reaching open-live is what it waits for.
    const landed = seat.chain.find(step => step.step === "open-live");
    const sending = seat.chain.find(step => step.step === "flowing");
    if (landed?.state === "done" && sending && sending.state !== "pending") {
      reached = true;
      break;
    }
  }
  await new Promise(resolve => setTimeout(resolve, 2000));
}

if (reached) {
  const seat = await seatOf(invite.seat);
  const flowing = seat.chain.find(step => step.step === "flowing");
  console.log("the guest's camera reached open-live as a source");
  if (flowing.state !== "done") {
    console.log("  weave does not read the stream as flowing yet: nothing is pulling the");
    console.log("  gateway's SRT output until a production consuming this source is active");
  }
} else {
  console.log("timed out before the source reached open-live");
}
if (args.stay) {
  console.log("staying up; ctrl-c to drop the guest");
  await new Promise(() => {});
}
await browser.close();
process.exit(reached ? 0 : 1);

async function seatOf(id) {
  const body = await get(`${gateway}/api/seats`);
  return body.seats.find(seat => seat.seat === id);
}

async function get(url) {
  const response = await fetch(url, { signal: AbortSignal.timeout(5000) });
  if (!response.ok) throw new Error(`GET ${url} → ${response.status}`);
  return response.json();
}

async function post(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(5000),
  });
  if (!response.ok) throw new Error(`POST ${url} → ${response.status} ${await response.text()}`);
  return response.json();
}

function parse(argv) {
  const parsed = {};
  for (let i = 0; i < argv.length; i += 1) {
    if (!argv[i].startsWith("--")) continue;
    const key = argv[i].slice(2);
    const next = argv[i + 1];
    if (next && !next.startsWith("--")) {
      parsed[key] = next;
      i += 1;
    } else {
      parsed[key] = true;
    }
  }
  return parsed;
}
