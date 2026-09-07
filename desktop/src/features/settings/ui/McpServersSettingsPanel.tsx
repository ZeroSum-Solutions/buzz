import * as React from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Plus, ShieldAlert, Trash2 } from "lucide-react";

import {
  deleteMcpRegistryServer,
  listMcpRegistryServers,
  saveMcpRegistryServer,
  type McpRegistryEntry,
} from "@/shared/api/tauriMcpRegistry";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";
import { cn } from "@/shared/lib/cn";

import {
  approvalSummary,
  draftProblem,
  draftToInput,
  emptyDraft,
  entryToDraft,
  type McpServerDraft,
} from "./mcpRegistryLogic";
import { SettingsOptionGroup } from "./SettingsOptionGroup";

/** Query key for the registry document, so a save invalidates every reader. */
export const MCP_REGISTRY_QUERY_KEY = ["mcp-registry"] as const;

/** Read the registry document and each entry's status. */
export function useMcpRegistryQuery() {
  return useQuery({
    queryKey: MCP_REGISTRY_QUERY_KEY,
    queryFn: listMcpRegistryServers,
  });
}

function TransportChoice({
  draft,
  onChange,
}: {
  draft: McpServerDraft;
  onChange: (draft: McpServerDraft) => void;
}) {
  return (
    <div className="flex gap-2" role="radiogroup" aria-label="Transport">
      {(["stdio", "http"] as const).map((transport) => (
        <Button
          aria-checked={draft.transport === transport}
          key={transport}
          onClick={() => onChange({ ...draft, transport })}
          role="radio"
          size="sm"
          type="button"
          variant={draft.transport === transport ? "default" : "outline"}
        >
          {transport === "stdio" ? "Local process" : "HTTP endpoint"}
        </Button>
      ))}
    </div>
  );
}

/**
 * The approve step.
 *
 * Nothing is written until the operator has seen the exact command line, or
 * the exact URL, plus the *name* of every credential the entry will resolve.
 * The values themselves are in `draft.secrets` and reach only the save call.
 */
function ApproveStep({
  draft,
  onBack,
  onConfirm,
  pending,
}: {
  draft: McpServerDraft;
  onBack: () => void;
  onConfirm: () => void;
  pending: boolean;
}) {
  const summary = approvalSummary(draft);
  return (
    <div className="space-y-3 px-4 py-4" data-testid="mcp-server-approve">
      <p className="text-sm font-medium">{summary.headline}</p>
      <pre
        className="overflow-x-auto rounded-lg bg-muted/40 px-3 py-2 font-mono text-xs"
        data-testid="mcp-server-approve-target"
      >
        {summary.target}
      </pre>
      {summary.references.length > 0 ? (
        <div className="space-y-1">
          <p className="text-sm text-muted-foreground/80">
            It resolves these credentials by name. The values stay in the
            keychain and are never written to a config file:
          </p>
          <ul
            className="space-y-0.5 font-mono text-xs text-muted-foreground"
            data-testid="mcp-server-approve-references"
          >
            {summary.references.map((reference) => (
              <li key={reference}>{reference}</li>
            ))}
          </ul>
        </div>
      ) : null}
      {summary.newSecrets.length > 0 ? (
        <p className="text-sm text-muted-foreground/80">
          {`Saving stores ${summary.newSecrets.length} new credential value${summary.newSecrets.length === 1 ? "" : "s"} (${summary.newSecrets.join(", ")}). You will not be able to read them back.`}
        </p>
      ) : null}
      <div className="flex gap-2">
        <Button disabled={pending} onClick={onConfirm} size="sm" type="button">
          Approve and save
        </Button>
        <Button
          disabled={pending}
          onClick={onBack}
          size="sm"
          type="button"
          variant="ghost"
        >
          Back
        </Button>
      </div>
    </div>
  );
}

function ServerForm({
  draft,
  onCancel,
  onChange,
  onReview,
}: {
  draft: McpServerDraft;
  onCancel: () => void;
  onChange: (draft: McpServerDraft) => void;
  onReview: () => void;
}) {
  const problem = draftProblem(draft);
  return (
    <div className="space-y-3 px-4 py-4" data-testid="mcp-server-form">
      <TransportChoice draft={draft} onChange={onChange} />
      <div className="grid gap-2 sm:grid-cols-2">
        <label className="space-y-1 text-sm" htmlFor="mcp-server-id">
          <span className="text-muted-foreground/80">Id</span>
          <Input
            id="mcp-server-id"
            onChange={(event) => onChange({ ...draft, id: event.target.value })}
            placeholder="fake-server"
            value={draft.id}
          />
        </label>
        <label className="space-y-1 text-sm" htmlFor="mcp-server-name">
          <span className="text-muted-foreground/80">Name</span>
          <Input
            id="mcp-server-name"
            onChange={(event) =>
              onChange({ ...draft, name: event.target.value })
            }
            placeholder="fake-server"
            value={draft.name}
          />
        </label>
      </div>
      {draft.transport === "stdio" ? (
        <>
          <label className="space-y-1 text-sm" htmlFor="mcp-server-command">
            <span className="text-muted-foreground/80">
              Command (absolute path)
            </span>
            <Input
              id="mcp-server-command"
              onChange={(event) =>
                onChange({ ...draft, command: event.target.value })
              }
              placeholder="/usr/local/bin/fake-mcp"
              value={draft.command}
            />
          </label>
          <label className="space-y-1 text-sm" htmlFor="mcp-server-args">
            <span className="text-muted-foreground/80">
              Arguments, one per line
            </span>
            <textarea
              id="mcp-server-args"
              className="min-h-20 w-full rounded-md border border-border/70 bg-background/70 px-3 py-2 font-mono text-xs"
              onChange={(event) =>
                onChange({ ...draft, argsText: event.target.value })
              }
              value={draft.argsText}
            />
          </label>
        </>
      ) : (
        <>
          <label className="space-y-1 text-sm" htmlFor="mcp-server-url">
            <span className="text-muted-foreground/80">Upstream URL</span>
            <Input
              id="mcp-server-url"
              onChange={(event) =>
                onChange({ ...draft, url: event.target.value })
              }
              placeholder="https://mcp.example/v1"
              value={draft.url}
            />
          </label>
          <div className="grid gap-2 sm:grid-cols-2">
            <label
              className="space-y-1 text-sm"
              htmlFor="mcp-server-auth-scheme"
            >
              <span className="text-muted-foreground/80">Auth scheme</span>
              <Input
                id="mcp-server-auth-scheme"
                onChange={(event) =>
                  onChange({ ...draft, authScheme: event.target.value })
                }
                value={draft.authScheme}
              />
            </label>
            <label
              className="space-y-1 text-sm"
              htmlFor="mcp-server-credential-name"
            >
              <span className="text-muted-foreground/80">Credential name</span>
              <Input
                id="mcp-server-credential-name"
                onChange={(event) =>
                  onChange({ ...draft, authSecretName: event.target.value })
                }
                placeholder="remote-token"
                value={draft.authSecretName}
              />
            </label>
          </div>
          {draft.authSecretName.length > 0 ? (
            <label
              className="space-y-1 text-sm"
              htmlFor="mcp-server-credential-value"
            >
              <span className="text-muted-foreground/80">
                Credential value (stored once, never shown again)
              </span>
              <Input
                id="mcp-server-credential-value"
                autoComplete="off"
                data-testid="mcp-server-secret-value"
                onChange={(event) =>
                  onChange({
                    ...draft,
                    secrets: {
                      ...draft.secrets,
                      [draft.authSecretName]: event.target.value,
                    },
                  })
                }
                type="password"
                value={draft.secrets[draft.authSecretName] ?? ""}
              />
            </label>
          ) : null}
        </>
      )}
      {problem !== null ? (
        <p className="text-sm text-amber-600" data-testid="mcp-server-problem">
          {problem}
        </p>
      ) : null}
      <div className="flex gap-2">
        <Button
          disabled={problem !== null}
          onClick={onReview}
          size="sm"
          type="button"
        >
          Review
        </Button>
        <Button onClick={onCancel} size="sm" type="button" variant="ghost">
          Cancel
        </Button>
      </div>
    </div>
  );
}

function ServerRow({
  entry,
  onDelete,
  onEdit,
}: {
  entry: McpRegistryEntry;
  onDelete: () => void;
  onEdit: () => void;
}) {
  return (
    <div
      className={cn("px-4 py-3 text-sm", entry.rejection && "bg-amber-500/5")}
      data-testid={`mcp-server-row-${entry.id}`}
    >
      <div className="flex items-center justify-between gap-3">
        <div className="min-w-0">
          <p className="font-medium">{entry.name}</p>
          <p className="truncate font-mono text-xs text-muted-foreground/70">
            {entry.transport === "stdio"
              ? [entry.command, ...entry.args].join(" ")
              : entry.url}
          </p>
        </div>
        <div className="flex shrink-0 gap-1">
          <Button onClick={onEdit} size="sm" type="button" variant="ghost">
            Edit
          </Button>
          <Button
            aria-label={`Delete ${entry.name}`}
            onClick={onDelete}
            size="sm"
            type="button"
            variant="ghost"
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </div>
      {entry.rejection !== null ? (
        <p
          className="mt-2 flex items-start gap-1.5 text-xs text-amber-600"
          data-testid={`mcp-server-rejection-${entry.id}`}
        >
          <ShieldAlert aria-hidden="true" className="mt-0.5 h-3.5 w-3.5" />
          <span>{entry.rejection}</span>
        </p>
      ) : null}
    </div>
  );
}

/**
 * Settings → Agents → MCP servers.
 *
 * The registry document is the operator's; this panel edits it and then adopts
 * a configuration generation, which is the single write that changes what any
 * agent spawns with. Every entry carries the loader's own status string, so
 * what the panel shows beside a disabled entry is the message its agents
 * refuse to start with.
 */
export function McpServersSettingsPanel() {
  const registry = useMcpRegistryQuery();
  const queryClient = useQueryClient();
  const [draft, setDraft] = React.useState<McpServerDraft | null>(null);
  const [approving, setApproving] = React.useState(false);
  const [failure, setFailure] = React.useState<string | null>(null);
  // The most recent save's own `refused` list. `list_mcp_registry_servers`
  // never recomputes refusals — it always answers `refused: []` — so the
  // post-save `invalidate()` refetch below would silently erase this the
  // instant it resolved if the panel read `registry.data?.refused` instead.
  const [saveRefusals, setSaveRefusals] = React.useState<[string, string][]>(
    [],
  );

  const invalidate = () => {
    void queryClient.invalidateQueries({ queryKey: MCP_REGISTRY_QUERY_KEY });
  };

  const save = useMutation({
    mutationFn: (next: McpServerDraft) =>
      saveMcpRegistryServer(draftToInput(next), next.secrets),
    onSuccess: (view) => {
      setDraft(null);
      setApproving(false);
      setFailure(null);
      setSaveRefusals(view.refused);
      invalidate();
    },
    // Surfaced, never swallowed: a failed convergence leaves the previous
    // generation adopted, and the operator has to be told that what they saved
    // is not what their agents are running.
    onError: (error: unknown) => setFailure(String(error)),
  });

  const remove = useMutation({
    mutationFn: (id: string) => deleteMcpRegistryServer(id),
    onSuccess: () => {
      setFailure(null);
      invalidate();
    },
    onError: (error: unknown) => setFailure(String(error)),
  });

  const servers = registry.data?.servers ?? [];
  const refused = saveRefusals;

  return (
    <SettingsOptionGroup
      data-testid="settings-mcp-servers"
      description="Tools your agents can use. Each one runs through the bundled launcher with an environment built from empty."
      headerAction={
        draft === null ? (
          <Button
            onClick={() => {
              setDraft(emptyDraft());
              setApproving(false);
            }}
            size="sm"
            type="button"
            variant="outline"
          >
            <Plus className="h-4 w-4" /> Add server
          </Button>
        ) : null
      }
      title="MCP servers"
    >
      {registry.isError ? (
        <p className="px-4 py-3 text-sm text-amber-600" role="alert">
          {String(registry.error)}
        </p>
      ) : null}
      {failure !== null ? (
        <p
          className="px-4 py-3 text-sm text-amber-600"
          data-testid="mcp-registry-failure"
          role="alert"
        >
          {failure}
        </p>
      ) : null}
      {refused.length > 0 ? (
        <div
          className="border-b border-border/40 bg-amber-500/10 px-4 py-3 text-sm text-amber-600 dark:text-amber-400"
          data-testid="mcp-registry-refusals"
          role="alert"
        >
          <div className="flex items-start gap-2">
            <ShieldAlert
              aria-hidden="true"
              className="mt-0.5 h-4 w-4 shrink-0"
            />
            <div className="space-y-1">
              <p className="font-medium">
                Some agents could not apply their MCP server settings:
              </p>
              <ul className="list-inside list-disc space-y-0.5 text-xs">
                {refused.map(([agentId, reason]) => (
                  <li key={agentId}>
                    <span className="font-mono">{agentId}</span>: {reason}
                  </li>
                ))}
              </ul>
            </div>
          </div>
        </div>
      ) : null}
      {servers.length === 0 && draft === null ? (
        <p className="px-4 py-6 text-sm text-muted-foreground/70">
          No MCP servers yet.
        </p>
      ) : null}
      {servers.map((entry) => (
        <ServerRow
          entry={entry}
          key={entry.id}
          onDelete={() => remove.mutate(entry.id)}
          onEdit={() => {
            setDraft(entryToDraft(entry));
            setApproving(false);
          }}
        />
      ))}
      {draft !== null && !approving ? (
        <ServerForm
          draft={draft}
          onCancel={() => setDraft(null)}
          onChange={setDraft}
          onReview={() => setApproving(true)}
        />
      ) : null}
      {draft !== null && approving ? (
        <ApproveStep
          draft={draft}
          onBack={() => setApproving(false)}
          onConfirm={() => save.mutate(draft)}
          pending={save.isPending}
        />
      ) : null}
    </SettingsOptionGroup>
  );
}
