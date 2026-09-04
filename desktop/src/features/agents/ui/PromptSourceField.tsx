import * as React from "react";

import { setPromptSourceAndReload } from "@/shared/api/tauriPersonas";
import { cn } from "@/shared/lib/cn";
import { Button } from "@/shared/ui/button";
import { Input } from "@/shared/ui/input";
import {
  PERSONA_FIELD_CONTROL_CLASS,
  PERSONA_FIELD_SHELL_CLASS,
  PERSONA_LABEL_OPTIONAL_CLASS,
} from "./agentConfigOptions";
import {
  canClearPromptSource,
  canReloadPromptSource,
  promptSourceStatusMessage,
} from "./promptSourceActions";

type PromptSourceFieldProps = {
  /** The definition being edited. The field is edit-mode only. */
  definitionId: string;
  disabled: boolean;
  /** Applies the reloaded text to the open dialog's instructions field. */
  onPromptReloaded: (prompt: string) => void;
};

/**
 * Bind an agent's instructions to a file on this machine and pull the file's
 * text into the definition.
 *
 * The path is machine-local: only the prompt text is published. Reload is
 * disabled until a path is typed; Clear is disabled until a binding exists,
 * so neither button offers an action that has nothing to act on.
 */
export function PromptSourceField({
  definitionId,
  disabled,
  onPromptReloaded,
}: PromptSourceFieldProps) {
  const [path, setPath] = React.useState("");
  const [storedPath, setStoredPath] = React.useState<string | null>(null);
  const [isPending, setIsPending] = React.useState(false);
  const [notice, setNotice] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);

  const busy = disabled || isPending;
  const reloadEnabled = canReloadPromptSource(path, busy);
  const clearEnabled = canClearPromptSource(storedPath !== null, busy);

  const run = async (nextPath: string | null) => {
    setIsPending(true);
    setError(null);
    setNotice(null);
    try {
      const result = await setPromptSourceAndReload(definitionId, nextPath);
      setStoredPath(result.path);
      if (result.path !== null) {
        setPath(result.path);
      }
      if (result.prompt !== null) {
        onPromptReloaded(result.prompt);
      }
      setNotice(promptSourceStatusMessage(result));
    } catch (caught) {
      // Surfaced, never swallowed: a refused path is the user's next action.
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setIsPending(false);
    }
  };

  return (
    <div className="space-y-1.5">
      <label
        className="text-sm font-medium text-foreground"
        htmlFor="persona-prompt-source"
      >
        Instructions file
        <span className={PERSONA_LABEL_OPTIONAL_CLASS}>Optional</span>
      </label>
      <div className="flex flex-wrap items-center gap-2">
        <div
          className={cn(
            "flex min-h-11 min-w-0 flex-1 items-center px-3",
            PERSONA_FIELD_SHELL_CLASS,
          )}
        >
          <Input
            aria-describedby="persona-prompt-source-status"
            autoCorrect="off"
            className={cn(
              "h-8 px-0 py-0 leading-6",
              PERSONA_FIELD_CONTROL_CLASS,
            )}
            disabled={busy}
            id="persona-prompt-source"
            onChange={(event) => setPath(event.target.value)}
            placeholder="/Users/you/agent-prompts/pm.md"
            value={path}
          />
        </div>
        <Button
          disabled={!reloadEnabled}
          onClick={() => void run(path)}
          type="button"
          variant="secondary"
        >
          Reload
        </Button>
        <Button
          disabled={!clearEnabled}
          onClick={() => {
            setPath("");
            void run(null);
          }}
          type="button"
          variant="ghost"
        >
          Clear
        </Button>
      </div>
      <p
        aria-live="polite"
        className={cn(
          "text-xs leading-5",
          error ? "text-destructive" : "text-muted-foreground",
        )}
        id="persona-prompt-source-status"
      >
        {error ??
          notice ??
          "Read this agent's instructions from a file in your home folder."}
      </p>
    </div>
  );
}
