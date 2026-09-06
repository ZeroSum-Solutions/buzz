import * as React from "react";

import { getAgentMcpServers } from "@/shared/api/tauriMcpRegistry";
import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";

import { AgentMcpServersField } from "./AgentMcpServersField";

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
 * `null` is "this record has never been configured", which memo decision 8
 * keeps distinct from an empty list. A failed load stays `null` rather than
 * becoming `[]`: guessing the other state here would write the wrong one on
 * the next toggle.
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
  const [enabled, setEnabled] = React.useState<string[] | null>(null);

  React.useEffect(() => {
    if (!open) return;
    let current = true;
    void getAgentMcpServers(pubkey)
      .then((selection) => {
        if (current) setEnabled(selection);
      })
      .catch(() => {
        if (current) setEnabled(null);
      });
    return () => {
      current = false;
    };
  }, [open, pubkey]);

  return (
    <div className="space-y-2">
      <p className="text-sm font-medium">MCP servers</p>
      <AgentMcpServersField
        enabled={enabled}
        onEnabledChange={setEnabled}
        pubkey={pubkey}
        runtime={runtime}
      />
    </div>
  );
}
