import { signal } from "@preact/signals";
import {
  CLOSE_UNAUTHORIZED,
  type EventName,
  type Events,
  type Hello,
  type MethodName,
  type PairResponse,
  type Params,
  type Result,
  type SessionResponse,
} from "./protocol";

// ---------------------------------------------------------------------------
// Token storage & HTTP

const TOKEN_KEY = "zed-mobile.token";
const DEVICE_NAME_KEY = "zed-mobile.device-name";

function readStorage(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStorage(key: string, value: string | null) {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    // Private mode or storage disabled; the session simply won't persist.
  }
}

export const token = signal<string | null>(readStorage(TOKEN_KEY));
export const deviceName = signal<string | null>(readStorage(DEVICE_NAME_KEY));

export function setToken(value: string | null) {
  writeStorage(TOKEN_KEY, value);
  token.value = value;
}

export function setDeviceName(value: string | null) {
  writeStorage(DEVICE_NAME_KEY, value);
  deviceName.value = value;
}

export class HttpError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
  }
}

async function errorFrom(response: Response): Promise<HttpError> {
  let message = `${response.status} ${response.statusText}`.trim();
  try {
    const body = (await response.json()) as { error?: unknown };
    if (typeof body.error === "string" && body.error) message = body.error;
  } catch {
    // Not JSON; keep the status line.
  }
  return new HttpError(message, response.status);
}

export async function pairDevice(code: string, name: string): Promise<string> {
  const response = await fetch("/api/pair", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code, device_name: name }),
  });
  if (!response.ok) throw await errorFrom(response);
  const body = (await response.json()) as PairResponse;
  if (typeof body.token !== "string" || !body.token) throw new HttpError("The server sent no token", 500);
  return body.token;
}

/** Fetch with the bearer token. Throws `HttpError` on non-2xx responses. */
export async function authedFetch(path: string, init: RequestInit = {}): Promise<Response> {
  const headers = new Headers(init.headers);
  if (token.value) headers.set("Authorization", `Bearer ${token.value}`);
  if (init.body && !headers.has("Content-Type")) headers.set("Content-Type", "application/json");
  const response = await fetch(path, { ...init, headers });
  if (!response.ok) throw await errorFrom(response);
  return response;
}

/** `null` when the session couldn't be checked (e.g. offline). */
export async function fetchSession(): Promise<SessionResponse | "unauthorized" | null> {
  try {
    const response = await authedFetch("/api/session");
    return (await response.json()) as SessionResponse;
  } catch (error) {
    if (error instanceof HttpError && error.status === 401) return "unauthorized";
    return null;
  }
}

// ---------------------------------------------------------------------------
// WebSocket

export type ConnectionStatus =
  /** Not started, or stopped (no token). */
  | "idle"
  | "connecting"
  | "authenticating"
  /** Authenticated and `hello` received. */
  | "ready"
  /** Lost; a reconnect is scheduled. */
  | "offline";

export class RequestError extends Error {
  constructor(
    message: string,
    readonly method: string,
    readonly kind: "server" | "timeout" | "connection",
  ) {
    super(message);
  }
}

interface PendingRequest {
  method: string;
  resolve: (value: unknown) => void;
  reject: (error: RequestError) => void;
  timer: ReturnType<typeof setTimeout>;
}

export interface RequestOptions {
  /** Milliseconds before the request fails (default 20s). */
  timeout?: number;
  /**
   * Only send on this connection generation, failing otherwise. Used for
   * requests that refer to per-connection state such as buffer versions.
   */
  generation?: number;
}

const DEFAULT_TIMEOUT = 20_000;
const AUTH_TIMEOUT = 10_000;
const HEALTH_CHECK_TIMEOUT = 4_000;

type Handler<E extends EventName> = (data: Events[E]) => void;

export class Connection {
  readonly status = signal<ConnectionStatus>("idle");
  readonly hello = signal<Hello | null>(null);
  /** Increments every time a connection becomes ready. */
  readonly generationSignal = signal(0);
  /** Set when the server rejected the token, so the UI can explain why. */
  readonly revoked = signal(false);

  private socket: WebSocket | null = null;
  private nextId = 1;
  private pending = new Map<number, PendingRequest>();
  private handlers = new Map<string, Set<(data: unknown) => void>>();
  private readyWaiters = new Set<() => void>();
  private attempts = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private wanted = false;
  private checkingHealth = false;

  get generation(): number {
    return this.generationSignal.peek();
  }

  get isReady(): boolean {
    return this.status.peek() === "ready";
  }

  start() {
    this.wanted = true;
    this.revoked.value = false;
    if (!this.socket) this.connect();
  }

  stop() {
    this.wanted = false;
    this.clearReconnectTimer();
    const socket = this.socket;
    this.socket = null;
    socket?.close(1000);
    this.failPending("Disconnected");
    this.status.value = "idle";
  }

  /** Called when the app becomes visible again or the network comes back. */
  resume() {
    if (!this.wanted || !token.peek()) return;
    if (this.status.peek() === "ready") {
      void this.checkHealth();
    } else if (this.status.peek() === "offline") {
      this.attempts = 0;
      this.connect();
    }
  }

  on<E extends EventName>(event: E, handler: Handler<E>): () => void {
    let set = this.handlers.get(event);
    if (!set) {
      set = new Set();
      this.handlers.set(event, set);
    }
    const wrapped = handler as (data: unknown) => void;
    set.add(wrapped);
    return () => set.delete(wrapped);
  }

  async request<M extends MethodName>(
    method: M,
    params: Params<M>,
    options: RequestOptions = {},
  ): Promise<Result<M>> {
    const timeout = options.timeout ?? DEFAULT_TIMEOUT;
    if (options.generation !== undefined) {
      if (options.generation !== this.generation || !this.isReady) {
        throw new RequestError("The connection was reset", method, "connection");
      }
    } else if (!this.isReady) {
      await this.waitUntilReady(timeout, method);
    }
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new RequestError("Not connected to Zed", method, "connection");
    }
    const id = this.nextId++;
    return new Promise<Result<M>>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new RequestError(`Zed didn't respond to ${method}`, method, "timeout"));
      }, timeout);
      this.pending.set(id, {
        method,
        resolve: resolve as (value: unknown) => void,
        reject,
        timer,
      });
      socket.send(JSON.stringify({ id, method, params: params ?? {} }));
    });
  }

  private waitUntilReady(timeout: number, method: string): Promise<void> {
    if (!this.wanted) {
      return Promise.reject(new RequestError("Not connected to Zed", method, "connection"));
    }
    return new Promise((resolve, reject) => {
      const done = () => {
        clearTimeout(timer);
        this.readyWaiters.delete(done);
        resolve();
      };
      const timer = setTimeout(() => {
        this.readyWaiters.delete(done);
        reject(new RequestError("Not connected to Zed", method, "connection"));
      }, timeout);
      this.readyWaiters.add(done);
    });
  }

  private connect() {
    if (!this.wanted) return;
    const currentToken = token.peek();
    if (!currentToken) {
      this.status.value = "idle";
      return;
    }
    this.clearReconnectTimer();
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    let socket: WebSocket;
    try {
      socket = new WebSocket(`${scheme}://${location.host}/api/ws`);
    } catch {
      this.status.value = "offline";
      this.scheduleReconnect();
      return;
    }
    this.socket = socket;
    this.status.value = "connecting";
    let authenticated = false;
    const authTimer = setTimeout(() => {
      if (!authenticated && this.socket === socket) this.dropSocket(socket);
    }, AUTH_TIMEOUT);

    socket.onopen = () => {
      if (this.socket !== socket) return;
      this.status.value = "authenticating";
      socket.send(JSON.stringify({ type: "auth", token: currentToken }));
    };
    socket.onmessage = (message) => {
      if (this.socket !== socket || typeof message.data !== "string") return;
      let frame: Record<string, unknown>;
      try {
        frame = JSON.parse(message.data) as Record<string, unknown>;
      } catch {
        console.warn("Ignoring a malformed frame", message.data);
        return;
      }
      if (!authenticated) {
        if (frame.type === "auth_ok") {
          authenticated = true;
          clearTimeout(authTimer);
          return;
        }
        if (frame.type === undefined) {
          // Some servers skip `auth_ok` and go straight to API frames.
          authenticated = true;
          clearTimeout(authTimer);
        } else {
          return;
        }
      }
      this.handleFrame(frame);
    };
    socket.onclose = (event) => {
      clearTimeout(authTimer);
      if (this.socket !== socket) return;
      this.socket = null;
      this.failPending("Connection lost");
      if (event.code === CLOSE_UNAUTHORIZED) {
        this.wanted = false;
        this.revoked.value = true;
        this.status.value = "idle";
        setToken(null);
        return;
      }
      this.status.value = "offline";
      this.scheduleReconnect();
    };
    socket.onerror = () => {
      // A close event always follows.
    };
  }

  private handleFrame(frame: Record<string, unknown>) {
    if (typeof frame.id === "number" && ("result" in frame || "error" in frame)) {
      const pending = this.pending.get(frame.id);
      if (!pending) return;
      this.pending.delete(frame.id);
      clearTimeout(pending.timer);
      if ("error" in frame) {
        pending.reject(new RequestError(String(frame.error), pending.method, "server"));
      } else {
        pending.resolve(frame.result);
      }
      return;
    }
    if (typeof frame.event !== "string") return;
    if (frame.event === "hello") {
      this.hello.value = frame.data as Hello;
      this.attempts = 0;
      this.generationSignal.value = this.generation + 1;
      this.status.value = "ready";
      for (const waiter of Array.from(this.readyWaiters)) waiter();
    }
    const set = this.handlers.get(frame.event);
    if (!set) return;
    for (const handler of Array.from(set)) {
      try {
        handler(frame.data);
      } catch (error) {
        console.error(`Error handling ${frame.event}`, error);
      }
    }
  }

  private async checkHealth() {
    if (this.checkingHealth) return;
    this.checkingHealth = true;
    const socket = this.socket;
    try {
      await this.request("threads.list", {}, { timeout: HEALTH_CHECK_TIMEOUT, generation: this.generation });
    } catch (error) {
      // iOS keeps dead sockets "open" after the app was suspended.
      if (error instanceof RequestError && error.kind === "timeout" && socket && this.socket === socket) {
        this.dropSocket(socket);
      }
    } finally {
      this.checkingHealth = false;
    }
  }

  private dropSocket(socket: WebSocket) {
    if (this.socket !== socket) return;
    this.socket = null;
    socket.onclose = null;
    socket.close();
    this.failPending("Connection lost");
    this.status.value = "offline";
    this.attempts = 0;
    this.connect();
  }

  private failPending(reason: string) {
    for (const [id, pending] of this.pending) {
      clearTimeout(pending.timer);
      pending.reject(new RequestError(reason, pending.method, "connection"));
      this.pending.delete(id);
    }
  }

  private scheduleReconnect() {
    if (!this.wanted) return;
    this.clearReconnectTimer();
    const base = Math.min(10_000, 400 * 2 ** this.attempts);
    const delay = base * (0.75 + Math.random() * 0.5);
    this.attempts++;
    this.reconnectTimer = setTimeout(() => this.connect(), delay);
  }

  private clearReconnectTimer() {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }
}
