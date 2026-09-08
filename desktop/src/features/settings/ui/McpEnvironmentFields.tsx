import { MCP_REGISTRY_LIMITS, type McpServerDraft } from "./mcpRegistryLogic";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";

/** Environment references and one-way credential entry for a local server. */
export function McpEnvironmentFields({
  draft,
  onChange,
}: {
  draft: McpServerDraft;
  onChange: (draft: McpServerDraft) => void;
}) {
  const changeEnvironment = (env: McpServerDraft["env"]) => {
    const references = new Set(env.map((row) => row.reference));
    onChange({
      ...draft,
      env,
      secrets: Object.fromEntries(
        Object.entries(draft.secrets).filter(([name]) => references.has(name)),
      ),
    });
  };
  const update = (
    index: number,
    patch: Partial<McpServerDraft["env"][number]>,
  ) => {
    changeEnvironment(
      draft.env.map((row, current) =>
        current === index ? { ...row, ...patch } : row,
      ),
    );
  };
  return (
    <div className="space-y-2">
      {draft.env.map((row, index) => (
        <div
          className="space-y-2 rounded-md border border-border/70 p-3"
          key={row.rowKey ?? row.name}
        >
          <Input
            aria-label={`Variable name ${index + 1}`}
            placeholder="API_KEY"
            value={row.name}
            onChange={(event) => update(index, { name: event.target.value })}
          />
          {row.literal !== undefined ? (
            <Input
              aria-label={`Literal value ${index + 1}`}
              value={row.literal}
              onChange={(event) =>
                update(index, { literal: event.target.value })
              }
            />
          ) : (
            <>
              <Input
                aria-label={`Credential name ${index + 1}`}
                placeholder="api-key"
                value={row.reference}
                onChange={(event) =>
                  update(index, { reference: event.target.value })
                }
              />
              <Input
                aria-label={`Credential value ${index + 1}`}
                type="password"
                autoComplete="off"
                placeholder="Leave blank to keep the stored value"
                disabled={!row.reference}
                value={draft.secrets[row.reference] ?? ""}
                onChange={(event) =>
                  onChange({
                    ...draft,
                    secrets: {
                      ...draft.secrets,
                      [row.reference]: event.target.value,
                    },
                  })
                }
              />
            </>
          )}
          <Button
            type="button"
            variant="ghost"
            size="sm"
            aria-label={`Remove variable ${index + 1}`}
            onClick={() =>
              changeEnvironment(
                draft.env.filter((_, current) => current !== index),
              )
            }
          >
            Remove variable
          </Button>
        </div>
      ))}
      <Button
        type="button"
        variant="outline"
        size="sm"
        disabled={draft.env.length >= MCP_REGISTRY_LIMITS.envEntries}
        onClick={() =>
          onChange({
            ...draft,
            env: [
              ...draft.env,
              { rowKey: crypto.randomUUID(), name: "", reference: "" },
            ],
          })
        }
      >
        Add environment variable
      </Button>
    </div>
  );
}
