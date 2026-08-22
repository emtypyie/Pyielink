import WebSocket from "ws";

const port = Number(process.argv[2]);
const key = String(process.argv[3]);
const stabMs = Number(process.argv[4]) || 60000;

const results = [];
const check = (name, ok) => {
  results.push(ok);
  console.log(`${ok ? "ok" : "FAIL"}: ${name}`);
};
const frame = (ch, text) => {
  const p = Buffer.from(text);
  const f = Buffer.allocUnsafe(5 + p.length);
  f.writeUInt8(ch, 0);
  f.writeUInt32BE(p.length, 1);
  p.copy(f, 5);
  return f;
};

check("handoff accepted", Boolean(port && /^[0-9a-f]{64}$/.test(key)));

await new Promise((resolve) => {
  const badKey = key.replace(/.$/, (c) => (c === "0" ? "1" : "0"));
  const bad = new WebSocket(`ws://127.0.0.1:${port}`);
  bad.on("open", () => bad.send(JSON.stringify({ k: badKey })));
  bad.on("close", (code) => {
    check("wrong session key closed with 4001", code === 4001);
    resolve();
  });
  bad.on("error", () => {});
});

let authed = false;
let echoMatch = false;
let pingsSeen = 0;
let closedEarly = false;
let windowDone = false;
const echoToken = "mux-echo-" + Date.now();

await new Promise((resolve) => {
  const good = new WebSocket(`ws://127.0.0.1:${port}`);
  good.on("open", () => good.send(JSON.stringify({ k: key })));
  good.on("message", (data, isBinary) => {
    if (!isBinary) {
      if (authed) return;
      try {
        const ack = JSON.parse(Buffer.from(data).toString("utf8"));
        authed =
          ack.ok === true &&
          typeof ack.user === "string" &&
          ["user", "admin"].includes(ack.role);
        check(`server ack carries user+role (user=${ack.user}, role=${ack.role})`, authed);
        good.send(frame(1, echoToken));
      } catch {}
      return;
    }
    const b = Buffer.from(data);
    const payload = b.subarray(5).toString("utf8");
    if (payload === echoToken) echoMatch = true;
    if (b.readUInt8(0) === 1 && payload === "PING") {
      pingsSeen += 1;
      good.send(frame(1, "PONG"));
    }
  });
  good.on("close", (code) => {
    if (!windowDone) {
      closedEarly = true;
      console.log(`early close detected (code ${code})`);
      check("stayed connected through stability window", false);
    }
    resolve();
  });
  good.on("error", () => {});
  setTimeout(() => {
    windowDone = true;
    check("stayed connected through stability window", !closedEarly);
    check("auth ack received", authed);
    check(`control-channel mux round-trip echo (${echoToken})`, echoMatch);
    check(`answered ${pingsSeen} server heartbeats`, pingsSeen >= 2 && authed && !closedEarly);
    good.terminate();
  }, stabMs);
});

const pass = results.every(Boolean);
console.log(pass ? "DL-SUBPASS" : "DL-SUBFAIL");
process.exit(pass ? 0 : 1);
