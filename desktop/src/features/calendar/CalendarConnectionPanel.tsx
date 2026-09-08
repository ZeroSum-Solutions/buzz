import { useCalendar } from "./useCalendar";
import { Button } from "@/shared/ui/button";

export function CalendarConnectionPanel() {
  const { status, connect, disconnect, abandon, clear } = useCalendar();
  const [confirmation, setConfirmation] = useState<number | null>(null);
  const data = status.data;
  const pendingBlocksConnect =
    (data?.revocations ?? []).some((entry) => entry.state !== "abandoned") ||
    (data?.pending_revocations ?? 0) >= 8;
  const busy =
    connect.isPending ||
    disconnect.isPending ||
    abandon.isPending ||
    clear.isPending;
  const error =
    status.error ??
    connect.error ??
    disconnect.error ??
    abandon.error ??
    clear.error;
  return (
    <section className="space-y-4 p-6" aria-label="Google Calendar connection">
      <div>
        <h2 className="text-lg font-semibold">Google Calendar</h2>
        <p className="mt-1 text-sm text-muted-foreground">
          Your schedule, private to your Buzz account.
        </p>
      </div>
      {status.isPending ? <p role="status">Checking connection…</p> : null}
      {error ? (
        <p role="alert" className="text-sm text-destructive">
          {String(error)}
        </p>
      ) : null}
      {data?.error ? (
        <p role="status" className="text-sm text-muted-foreground">
          {data.error}
        </p>
      ) : null}
      {data?.connected ? (
        <div className="flex flex-wrap items-center gap-4">
          <span className="text-sm">Connected as {data.email}</span>
          <Button
            variant="outline"
            disabled={busy || data.generation === null}
            onClick={() => {
              if (data.generation !== null) disconnect.mutate(data.generation);
            }}
          >
            {disconnect.isPending ? "Disconnecting…" : "Disconnect"}
          </Button>
        </div>
      ) : data ? (
        <div className="space-y-3">
          {!data.configured ? (
            <p className="text-sm text-muted-foreground">
              Google connection setup is required for this installation.
            </p>
          ) : null}
          <Button
            disabled={busy || !data.configured || pendingBlocksConnect}
            onClick={() => connect.mutate()}
          >
            {connect.isPending
              ? "Waiting for Google…"
              : "Connect Google Calendar"}
          </Button>
        </div>
      ) : null}
      {data && data.pending_revocations > 0 ? (
        <p role="status" className="text-sm text-muted-foreground">
          Previous Google access removal needs attention. Disconnected calendars
          cannot be used in Buzz.
        </p>
      ) : null}
      {(data?.revocations ?? []).map((entry) => (
        <div key={entry.generation} className="space-y-3 rounded-lg border p-4">
          <p className="text-sm">
            {entry.state === "abandoned"
              ? "Automatic removal was stopped."
              : entry.state === "revocation_unconfirmed"
                ? "Google access removal could not be confirmed."
                : "Buzz is retrying Google access removal."}
          </p>
          {!entry.purge_confirmed ? (
            <p className="text-sm text-muted-foreground">
              Local calendar cleanup has not finished.
            </p>
          ) : null}
          {entry.state === "abandoned" ? (
            <Button
              variant="outline"
              disabled={busy || !entry.purge_confirmed}
              onClick={() => clear.mutate(entry.generation)}
            >
              Clear this cleanup record
            </Button>
          ) : confirmation === entry.generation ? (
            <div className="space-y-3">
              <p className="text-sm text-muted-foreground">
                Google may still allow access. Stopping retries lets you
                reconnect; it does not confirm that Google revoked the old
                connection. You can remove Buzz access in your Google Account
                settings.
              </p>
              <div className="flex flex-wrap gap-2">
                <Button
                  variant="outline"
                  disabled={busy}
                  onClick={() => setConfirmation(null)}
                >
                  Keep cleanup
                </Button>
                <Button
                  disabled={busy}
                  onClick={() =>
                    abandon.mutate(entry.generation, {
                      onSuccess: () => setConfirmation(null),
                    })
                  }
                >
                  Stop retries and allow reconnect
                </Button>
              </div>
            </div>
          ) : (
            <Button
              variant="outline"
              disabled={busy}
              onClick={() => setConfirmation(entry.generation)}
            >
              Stop retrying cleanup…
            </Button>
          )}
        </div>
      ))}
    </section>
  );
}
import { useState } from "react";
