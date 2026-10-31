import { useEffect, useState } from "preact/hooks";
import { authedFetch, deviceName, setToken } from "./connection";
import type { PushKeyResponse } from "./protocol";
import { conn, errorMessage, toast } from "./state";
import { haptic, Icon, isIOS, isStandalone, Sheet, Spinner, Switch } from "./ui";

type PushState =
  | { kind: "checking" }
  | { kind: "unsupported"; reason: string }
  | { kind: "needs-install" }
  | { kind: "denied" }
  | { kind: "off" }
  | { kind: "on" };

function base64UrlToBytes(value: string): Uint8Array<ArrayBuffer> {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/") + "=".repeat((4 - (value.length % 4)) % 4);
  const binary = atob(padded);
  const bytes = new Uint8Array(new ArrayBuffer(binary.length));
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function pushSupported(): boolean {
  return "serviceWorker" in navigator && "PushManager" in window && "Notification" in window;
}

async function currentRegistration(): Promise<ServiceWorkerRegistration | null> {
  if (!("serviceWorker" in navigator)) return null;
  return (await navigator.serviceWorker.getRegistration()) ?? null;
}

async function detectPushState(): Promise<PushState> {
  if (!pushSupported()) {
    if (isIOS && !isStandalone()) return { kind: "needs-install" };
    return { kind: "unsupported", reason: "This browser doesn't support push notifications." };
  }
  if (isIOS && !isStandalone()) return { kind: "needs-install" };
  if (Notification.permission === "denied") return { kind: "denied" };
  const registration = await currentRegistration();
  if (!registration) {
    return {
      kind: "unsupported",
      reason: import.meta.env.DEV
        ? "The service worker isn't registered in development builds."
        : "The service worker isn't ready yet. Try again in a moment.",
    };
  }
  const subscription = await registration.pushManager.getSubscription();
  return subscription && Notification.permission === "granted" ? { kind: "on" } : { kind: "off" };
}

async function unsubscribePush() {
  const registration = await currentRegistration();
  const subscription = await registration?.pushManager.getSubscription();
  if (!subscription) return;
  try {
    await authedFetch("/api/push/unsubscribe", {
      method: "POST",
      body: JSON.stringify({ endpoint: subscription.endpoint }),
    });
  } finally {
    await subscription.unsubscribe();
  }
}

export async function unpairDevice() {
  try {
    await unsubscribePush();
  } catch (error) {
    console.warn("Couldn't unsubscribe from push", error);
  }
  // The secret is shared with every other client, so it can only be revoked
  // by changing it in Zed; forgetting it here is all this device can do.
  conn.stop();
  setToken(null);
}

export function SettingsSheet({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [push, setPush] = useState<PushState>({ kind: "checking" });
  const [busy, setBusy] = useState(false);
  // Fetched up front so subscribing can happen synchronously in the tap
  // handler, which Safari requires.
  const [registration, setRegistration] = useState<ServiceWorkerRegistration | null>(null);
  const [vapidKey, setVapidKey] = useState<string | null>(null);
  const hello = conn.hello.value;

  useEffect(() => {
    if (!open) return;
    void detectPushState().then(setPush);
    void currentRegistration().then(setRegistration);
    authedFetch("/api/push/key")
      .then((response) => response.json() as Promise<PushKeyResponse>)
      .then((body) => setVapidKey(body.public_key))
      .catch(() => setVapidKey(null));
  }, [open]);

  const enable = async () => {
    haptic();
    setBusy(true);
    try {
      let key = vapidKey;
      if (!key) {
        const response = await authedFetch("/api/push/key");
        key = ((await response.json()) as PushKeyResponse).public_key;
      }
      const target = registration ?? (await navigator.serviceWorker.ready);
      // `subscribe` asks for notification permission itself when needed.
      const subscription = await target.pushManager.subscribe({
        userVisibleOnly: true,
        applicationServerKey: base64UrlToBytes(key),
      });
      await authedFetch("/api/push/subscribe", {
        method: "POST",
        body: JSON.stringify(subscription.toJSON()),
      });
      setPush({ kind: "on" });
      toast("Notifications enabled");
    } catch (error) {
      if (typeof Notification !== "undefined" && Notification.permission === "denied") setPush({ kind: "denied" });
      else toast(`Couldn't enable notifications: ${errorMessage(error)}`, "error");
    } finally {
      setBusy(false);
    }
  };

  const disable = async () => {
    haptic();
    setBusy(true);
    try {
      await unsubscribePush();
      setPush({ kind: "off" });
    } catch (error) {
      toast(`Couldn't disable notifications: ${errorMessage(error)}`, "error");
    } finally {
      setBusy(false);
    }
  };

  const unpair = async () => {
    if (!confirm("Unpair this device? You'll need a new pairing code from Zed to reconnect.")) return;
    onClose();
    await unpairDevice();
  };

  return (
    <Sheet open={open} onClose={onClose} title="Settings">
      <div class="settings">
        <div class="form-section-title">Notifications</div>
        <div class="form-group">
          {push.kind === "needs-install" ? (
            <div class="form-note">
              <Icon name="info" size={18} />
              <div>
                On iPhone, notifications only work in the installed app. In Safari, tap <b>Share</b> → <b>Add to Home
                Screen</b>, open Zed from your Home Screen, and pair it there (the installed app has its own storage).
              </div>
            </div>
          ) : push.kind === "unsupported" ? (
            <div class="form-note">
              <Icon name="info" size={18} />
              <div>{push.reason}</div>
            </div>
          ) : push.kind === "denied" ? (
            <div class="form-note">
              <Icon name="alert" size={18} />
              <div>Notifications are blocked. Allow them for Zed in the system Settings app, then come back.</div>
            </div>
          ) : (
            <div class="form-row">
              <span class="form-row-label">Enable notifications</span>
              <span class="form-row-value">
                {busy || push.kind === "checking" ? (
                  <Spinner size={18} />
                ) : (
                  <Switch
                    checked={push.kind === "on"}
                    label="Enable notifications"
                    onChange={(value) => void (value ? enable() : disable())}
                  />
                )}
              </span>
            </div>
          )}
        </div>
        <div class="form-hint">Get notified when an agent finishes or needs your approval.</div>

        <div class="form-section-title">This device</div>
        <div class="form-group">
          <div class="form-row">
            <span class="form-row-label">Device name</span>
            <span class="form-row-value muted">{deviceName.value ?? "—"}</span>
          </div>
          <div class="form-row">
            <span class="form-row-label">Connection</span>
            <span class="form-row-value muted">{conn.status.value === "ready" ? "Connected" : "Reconnecting…"}</span>
          </div>
          {hello && (
            <div class="form-row">
              <span class="form-row-label">Zed</span>
              <span class="form-row-value muted">{hello.zed_version}</span>
            </div>
          )}
        </div>

        <div class="form-group">
          <button class="form-row destructive pressable" onClick={() => void unpair()}>
            <Icon name="logout" size={18} />
            <span class="form-row-label">Unpair this device</span>
          </button>
        </div>
      </div>
    </Sheet>
  );
}
