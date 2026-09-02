import assert from "node:assert/strict";
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";

const httpOrigin = required("VOICETEXT_GATEWAY_E2E_HTTP_ORIGIN");
const wsOrigin = required("VOICETEXT_GATEWAY_E2E_WS_ORIGIN");
const token = required("VOICETEXT_GATEWAY_E2E_TOKEN");
const fixture = fs.readFileSync(required("VOICETEXT_GATEWAY_E2E_OGG_FIXTURE"));

function required(name) {
  const value = process.env[name];
  assert.ok(value, `${name} is required`);
  return value;
}

function form(profile) {
  const body = new FormData();
  body.set("contract_version", profile.version);
  body.set("provider", profile.provider);
  body.set("model", profile.model);
  body.set("language", "multi");
  body.set("keyterms", JSON.stringify(["Quanta"]));
  body.set("file", new Blob([fixture], { type: "audio/ogg" }), "speaker-track.ogg");
  return body;
}

async function batch(profile, idempotencyKey) {
  console.error(`checking ${profile.provider} batch contract`);
  const headers = {
    authorization: `Bearer ${token}`,
    "x-idempotency-key": idempotencyKey,
  };
  const submitted = await fetch(`${httpOrigin}/api/v1/transcribe/batch`, {
    method: "POST", headers, body: form(profile),
  });
  assert.equal(submitted.status, 202);
  const pending = await submitted.json();
  assert.equal(pending.provider ?? profile.provider, profile.provider);
  assert.match(pending.job_id, /^[0-9a-f-]{36}$/);

  let completed;
  for (let attempt = 0; attempt < 100; attempt += 1) {
    const response = await fetch(
      `${httpOrigin}/api/v1/transcribe/batch/${pending.job_id}`,
      { headers: { authorization: `Bearer ${token}` } },
    );
    const body = await response.json();
    if (response.status === 200) {
      completed = body;
      break;
    }
    assert.equal(response.status, 202);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.ok(completed, "batch job did not complete");
  assert.equal(completed.result.provider, profile.provider);
  assert.equal(completed.result.model, profile.model);
  assert.equal(completed.result.text, "synthetic speech");

  const replay = await fetch(`${httpOrigin}/api/v1/transcribe/batch`, {
    method: "POST", headers, body: form(profile),
  });
  assert.equal(replay.status, 200);
  assert.deepEqual(await replay.json(), completed);
}

class RawWebSocket {
  constructor(url) {
    this.url = new URL(url);
    this.buffer = Buffer.alloc(0);
    this.waiters = [];
  }

  async connect() {
    const key = crypto.randomBytes(16).toString("base64");
    this.socket = net.createConnection(Number(this.url.port), this.url.hostname);
    this.socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.flush();
    });
    await new Promise((resolve, reject) => {
      this.socket.once("connect", resolve);
      this.socket.once("error", reject);
    });
    this.socket.write([
      `GET ${this.url.pathname} HTTP/1.1`,
      `Host: ${this.url.host}`,
      "Upgrade: websocket",
      "Connection: Upgrade",
      `Sec-WebSocket-Key: ${key}`,
      "Sec-WebSocket-Version: 13",
      `Authorization: Bearer ${token}`,
      "\r\n",
    ].join("\r\n"));
    const header = await this.untilHeader();
    assert.match(header, /^HTTP\/1\.1 101 /);
    const expected = crypto
      .createHash("sha1")
      .update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
      .digest("base64");
    assert.ok(
      header.toLowerCase().includes(`sec-websocket-accept: ${expected.toLowerCase()}`),
      "WebSocket accept digest did not match the request key",
    );
  }

  untilHeader() {
    return new Promise((resolve) => {
      const inspect = () => {
        const end = this.buffer.indexOf("\r\n\r\n");
        if (end < 0) return false;
        const header = this.buffer.subarray(0, end).toString("utf8");
        this.buffer = this.buffer.subarray(end + 4);
        resolve(header);
        return true;
      };
      if (!inspect()) this.waiters.push(inspect);
    });
  }

  flush() {
    this.waiters = this.waiters.filter((waiter) => !waiter());
  }

  send(opcode, value) {
    const payload = Buffer.isBuffer(value) ? value : Buffer.from(value);
    assert.ok(payload.length < 65536, "fixture frames must fit the auditable 16-bit form");
    const extended = payload.length >= 126;
    const maskOffset = extended ? 4 : 2;
    const payloadOffset = extended ? 8 : 6;
    const mask = crypto.randomBytes(4);
    const frame = Buffer.alloc(payloadOffset + payload.length);
    frame[0] = 0x80 | opcode;
    frame[1] = 0x80 | (extended ? 126 : payload.length);
    if (extended) frame.writeUInt16BE(payload.length, 2);
    mask.copy(frame, maskOffset);
    for (let index = 0; index < payload.length; index += 1) {
      frame[index + payloadOffset] = payload[index] ^ mask[index % 4];
    }
    this.socket.write(frame);
  }

  text(value) { this.send(1, value); }
  binary(value) { this.send(2, value); }

  nextJson() {
    return new Promise((resolve) => {
      const inspect = () => {
        if (this.buffer.length < 2) return false;
        assert.equal(this.buffer[1] & 0x80, 0, "server frames must not be masked");
        const marker = this.buffer[1] & 0x7f;
        let headerLength = 2;
        let length = marker;
        if (marker === 126) {
          if (this.buffer.length < 4) return false;
          headerLength = 4;
          length = this.buffer.readUInt16BE(2);
        } else if (marker === 127) {
          if (this.buffer.length < 10) return false;
          headerLength = 10;
          const wideLength = this.buffer.readBigUInt64BE(2);
          assert.ok(wideLength <= 1024n * 1024n, "server frame exceeds fixture bound");
          length = Number(wideLength);
        }
        if (this.buffer.length < headerLength + length) return false;
        const opcode = this.buffer[0] & 0x0f;
        const payload = this.buffer.subarray(headerLength, headerLength + length);
        this.buffer = this.buffer.subarray(headerLength + length);
        if (opcode === 8) throw new Error("gateway closed before contract completion");
        if (opcode !== 1) return inspect();
        resolve(JSON.parse(payload.toString("utf8")));
        return true;
      };
      if (!inspect()) this.waiters.push(inspect);
    });
  }

  close() {
    this.send(8, Buffer.alloc(0));
    this.socket.end();
  }
}

async function live(provider, model, sequence) {
  console.error(`checking ${provider} live contract`);
  const socket = new RawWebSocket(`${wsOrigin}/api/v1/transcribe/stream`);
  await socket.connect();
  socket.text(JSON.stringify({
    type: "config", provider, model, language: "multi",
    capabilities: ["finalize_ack"], channels: 1, protocol_v: 2,
    client_session_id: `123e4567-e89b-42d3-a456-${String(sequence).padStart(12, "0")}`,
    encoding: "opus", sample_rate: 48000, keyterms: ["Quanta"],
  }));
  const ready = await socket.nextJson();
  assert.equal(ready.type, "ready");
  assert.equal(ready.provider, provider);
  assert.equal(ready.model, model);
  let finalized = false;
  let sawTranscript = false;
  for (let sequence = 1; sequence <= 4; sequence += 1) {
    socket.binary(Buffer.from([0xf8, 0xff, 0xfe]));
    let acknowledged = false;
    for (let count = 0; count < 8 && !acknowledged; count += 1) {
      const event = await socket.nextJson();
      acknowledged = event.type === "ack" && event.seq === sequence;
      sawTranscript ||= event.type === "partial" || event.type === "final";
    }
    assert.ok(acknowledged, `live audio ${sequence} was not acknowledged in order`);
  }
  socket.text(JSON.stringify({ type: "finalize" }));
  for (let count = 0; count < 12 && !finalized; count += 1) {
    const event = await socket.nextJson();
    sawTranscript ||= event.type === "partial" || event.type === "final";
    finalized = event.type === "finalize_complete" && event.status === "flushed" && event.saw_result;
  }
  assert.ok(sawTranscript, "live session emitted no transcript evidence");
  assert.ok(finalized, "live session did not prove provider flush");
  socket.close();
}

for (let run = 0; run < 2; run += 1) {
  await batch(
    { version: "2", provider: "deepgram", model: "nova-3" },
    crypto.createHash("sha256").update(`deepgram-${run}`).digest("hex"),
  );
  await batch(
    { version: "3", provider: "elevenlabs", model: "scribe_v2" },
    crypto.createHash("sha256").update(`elevenlabs-${run}`).digest("hex"),
  );
  await live("deepgram", "nova-3", run * 2 + 1);
  await live("elevenlabs", "scribe_v2_realtime", run * 2 + 2);
}

console.log("exact TypeScript VoiceText production-composition contract passed");
