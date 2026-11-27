// Local runner for the visible-turn collector prototype (MUNCH-022).
//
// Drives a headless Chromium over the DevTools protocol on a PIPE
// (--remote-debugging-pipe): no TCP listener is opened. Each run uses a fresh
// temporary profile that is deleted afterwards. HTTP(S) and WebSocket traffic,
// loopback and IP literals included, goes to a proxy that refuses it; every
// hostname resolves to NOTFOUND; WebRTC may not send non-proxied UDP. The
// test suite asserts these against local listeners (HTTP, WebSocket, STUN
// over UDP). Other egress paths are not claimed beyond what is tested.
// Node built-ins only; no npm dependencies.
//
// This prototype is for synthetic fixtures. Pointing it at an authenticated
// provider page is out of scope until the owner approves a specific surface
// (see README.md).

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const COLLECTOR_SOURCE = readFileSync(join(HERE, "collector.js"), "utf8");

export function loadProfile(name) {
  return JSON.parse(readFileSync(join(HERE, "profiles", `${name}.json`), "utf8"));
}

export const ISOLATION_FLAGS = [
  "--headless",
  "--remote-debugging-pipe",
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-background-networking",
  "--disable-component-update",
  "--disable-sync",
  "--disable-extensions",
  "--disable-default-apps",
  "--metrics-recording-only",
  "--proxy-server=http://127.0.0.1:9",
  "--proxy-bypass-list=<-loopback>",
  "--host-resolver-rules=MAP * ~NOTFOUND",
  // The proxy covers HTTP(S)/WebSocket only (IP literals included) and the
  // resolver rules do not apply to IP literals; WebRTC could still send UDP
  // (e.g. STUN) to any address. Forbid all non-proxied UDP.
  "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
];

class Cdp {
  constructor(child) {
    this.child = child;
    this.nextId = 1;
    this.waiting = new Map();
    this.listeners = [];
    let buffer = "";
    child.stdio[4].on("data", (chunk) => {
      buffer += chunk.toString("utf8");
      let end;
      while ((end = buffer.indexOf("\0")) !== -1) {
        const message = JSON.parse(buffer.slice(0, end));
        buffer = buffer.slice(end + 1);
        if (message.id && this.waiting.has(message.id)) {
          const { resolve, reject } = this.waiting.get(message.id);
          this.waiting.delete(message.id);
          if (message.error) reject(new Error(`${message.error.message}`));
          else resolve(message.result);
        } else if (message.method) {
          for (const listener of this.listeners) listener(message);
        }
      }
    });
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      this.waiting.set(id, { resolve, reject });
      this.child.stdio[3].write(`${JSON.stringify(payload)}\0`);
    });
  }

  once(method, sessionId, timeoutMs = 10_000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timeout waiting for ${method}`)), timeoutMs);
      const listener = (message) => {
        if (message.method === method && message.sessionId === sessionId) {
          clearTimeout(timer);
          this.listeners = this.listeners.filter((l) => l !== listener);
          resolve(message.params);
        }
      };
      this.listeners.push(listener);
    });
  }
}

/** Launch an isolated headless browser; returns { cdp, close, profileDir }. */
export async function launch(executable) {
  const profileDir = mkdtempSync(join(tmpdir(), "sm-visible-turn-collector-"));
  // No display server: headless only, and nothing inherited from a desktop.
  const env = { ...process.env };
  delete env.DISPLAY;
  delete env.WAYLAND_DISPLAY;
  const child = spawn(executable, [...ISOLATION_FLAGS, `--user-data-dir=${profileDir}`], {
    stdio: ["ignore", "ignore", "ignore", "pipe", "pipe"],
    env,
  });
  const exited = new Promise((resolve) => child.once("exit", resolve));
  const cdp = new Cdp(child);
  await cdp.send("Browser.getVersion");
  return {
    cdp,
    profileDir,
    async close() {
      try {
        await Promise.race([cdp.send("Browser.close"), new Promise((r) => setTimeout(r, 2_000))]);
      } finally {
        if (child.exitCode === null) child.kill("SIGKILL");
        await exited;
        rmSync(profileDir, { recursive: true, force: true });
      }
    },
  };
}

/** Open a page in a new target and return a small page handle. */
export async function openPage(browser, url) {
  const { cdp } = browser;
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  await cdp.send("Page.enable", {}, sessionId);
  await cdp.send("Runtime.enable", {}, sessionId);
  const loaded = cdp.once("Page.loadEventFired", sessionId);
  await cdp.send("Page.navigate", { url }, sessionId);
  await loaded;
  return {
    async evaluate(expression) {
      const result = await cdp.send(
        "Runtime.evaluate",
        { expression, returnByValue: true, awaitPromise: true },
        sessionId,
      );
      if (result.exceptionDetails) {
        throw new Error(result.exceptionDetails.exception?.description ?? "page exception");
      }
      return result.result.value;
    },
  };
}

/** Inject the collector into a page and start observing. */
export async function installCollector(page, profile, provider) {
  await page.evaluate(COLLECTOR_SOURCE);
  await page.evaluate(
    `globalThis.__smCollector = globalThis.__sessionmunchVisibleTurnCollector.createCollector(${JSON.stringify(profile)}, ${JSON.stringify({ provider, surface: "web" })}); globalThis.__smCollector.start(); true`,
  );
}

/** Let MutationObserver callbacks settle, then take pending records. */
export async function drain(page) {
  await page.evaluate("new Promise((resolve) => setTimeout(resolve, 0))");
  return page.evaluate("globalThis.__smCollector.drain()");
}

export function toNdjson(records) {
  return records.map((record) => `${JSON.stringify(record)}\n`).join("");
}
