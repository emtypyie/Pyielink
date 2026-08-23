import { createHash } from "node:crypto";
import WebSocket from "ws";

// scripted file-transfer ws client for negative/sandbox tests:
//   node _dl_files.mjs <port> <key> deny   <remotePath>   -> expect error[denied]
//   node _dl_files.mjs <port> <key> toobig <name>         -> expect error[toobig]
const [, , portArg, key, cmd, arg] = process.argv;
const port = Number(portArg);

const frame = (ch, text) => {
  const p = Buffer.from(text);
  const f = Buffer.allocUnsafe(5 + p.length);
  f.writeUInt8(ch, 0);
  f.writeUInt32BE(p.length, 1);
  p.copy(f, 5);
  return f;
};

const outcome = await new Promise((resolve) => {
  const ws = new WebSocket(`ws://127.0.0.1:${port}`);
  const bail = (why) => {
    console.log(`FAIL: ${why}`);
    try { ws.terminate(); } catch {}
    resolve({ ok: false });
  };
  const guard = setTimeout(() => bail("timed out waiting for server verdict"), 15000);
  ws.on("error", () => {});
  ws.on("close", (code) => {
    if (!outcomeSeen) bail(`closed before verdict (code ${code})`);
  });
  let outcomeSeen = false;

  ws.on("message", (data, isBinary) => {
    if (!isBinary) {
      // plain JSON auth ack before mux traffic
      try {
        const ack = JSON.parse(Buffer.from(data).toString("utf8"));
        if (ack.ok !== true) return bail("auth ack not ok");
        return; // authenticated; now send the probe
      } catch {
        return bail("bad auth ack json");
      }
    }
    const b = Buffer.from(data);
    const ch = b.readUInt8(0);
    if (ch === 1 && b.subarray(5).toString() === "PING") {
      ws.send(frame(1, "PONG"));
      return;
    }
    if (ch !== 0x04) return;
    clearTimeout(guard);
    outcomeSeen = true;
    let msg;
    try {
      msg = JSON.parse(b.subarray(5).toString("utf8"));
    } catch {
      return resolve({ ok: false });
    }
    if (msg.t === "error") {
      const wantCode = cmd === "toobig" ? "toobig" : "denied";
      console.log(`server refused as expected: [${msg.code}] ${msg.msg}`);
      resolve({ ok: msg.code === wantCode });
    } else {
      console.log(`FAIL: expected error verdict, got t=${msg.t}`);
      resolve({ ok: false });
    }
    try { ws.close(1000); } catch {}
  });

  ws.on("open", () => {
    ws.send(JSON.stringify({ k: key }));
    // first non-binary reply triggers the probe below
    ws.once("message", (d, bin) => {
      if (bin) return;
      const ack = JSON.parse(Buffer.from(d).toString());
      if (ack.ok !== true) return bail("handshake rejected");
      if (cmd === "deny") {
        ws.send(frame(0x04, JSON.stringify({ t: "push", id: 7, name: arg, size: 4, sha256: "a".repeat(64) })));
      } else if (cmd === "toobig") {
        ws.send(frame(0x04, JSON.stringify({ t: "push", id: 7, name: arg || "big.bin", size: 3 * 1024 * 1024 * 1024, sha256: createHash("sha256").update("x").digest("hex") })));
      } else {
        bail(`unknown command '${cmd}'`);
      }
    });
  });
});

console.log(outcome.ok ? "FT-SUBPASS" : "FT-SUBFAIL");
process.exit(outcome.ok ? 0 : 1);
