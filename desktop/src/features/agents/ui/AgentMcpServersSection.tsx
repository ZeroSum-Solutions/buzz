import * as React from "react";

import { getAgentMcpServers } from "@/shared/api/tauriMcpRegistry";
import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";

import {
  AgentMcpServersField,
  type AgentMcpSelectionState,
} from "./AgentMcpServersField";

/**
 * The MCP registry section of the agent edit dialog (memo decision 8).
 *
 * The selection lives on this agent's record, so it belongs on the instance
 * surface — a definition has no pubkey to write to. Each toggle writes the
 * record and adopts a configuration generation, which is the single write that
 * changes what the agent's next spawn reads; staging that behind the dialog's
 * unrelated Save would leave the panel and the agent disagreeing about what is
 * enabled.
 *
 * `null` from the backend is "this record has never been configured", which
 * maps to an empty enabled list on load. A failed load moves to `{ status: "error" }`
 * and disables switches rather than guessing an empty list.
 */
export function AgentMcpServersSection({
  open,
  pubkey,
  runtime,
}: {
  open: boolean;
  pubkey: string;
  runtime: AcpRuntimeCatalogEntry | null;
}) {
  const [selection, setSelection] = React.useState<AgentMcpSelectionState>({
    status: "loading",
  });

  React.useEffect(() => {
    if (!open) return;
    let current = true;
    setSelection({ status: "loading" });
    void getAgentMcpServers(pubkey)
      .then((enabled) => {
        if (current) {
          setSelection({ status: "loaded", enabled: enabled ?? [] });
        }
      })
      .catch((error) => {
        if (current) {
          setSelection({ status: "error", error: String(error) });
        }
      });
    return () => {
      current = false;
    };
  }, [open, pubkey]);

  return (
    <div className="space-y-2">
      <p className="text-sm font-medium">MCP servers</p>
      <AgentMcpServersField
        onEnabledChange={(next) =>
          setSelection({ status: "loaded", enabled: next })
        }
        pubkey={pubkey}
        runtime={runtime}
        selection={selection}
      />
    </div>
  );
}
