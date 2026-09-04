import * as React from "react";

import {
  getPromptSource,
  setPromptSourceAndReload,
} from "@/shared/api/tauriPersonas";
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
  DIRTY_EXEMPT_ATTRIBUTE,
  promptSourceHint,
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
 * The path is machine-local: only the prompt text is published, and typing it
 * leaves the parent dialog's unsaved-changes state untouched. Reload is
 * disabled until a path is typed.
 *
 * On open the field seeds itself from the stored binding, so re-opening the
 * dialog shows which file feeds this agent instead of an empty box the
 * operator has to retype an absolute path into. Clear stays available even
 * when the seed found nothing — a seed that failed, or a sidecar written by
 * another window since, must not make an existing binding unclearable.
 */
export function PromptSourceField({
  definitionId,
  disabled,
  onPromptReloaded,
}: PromptSourceFieldProps) {
  const [path, setPath] = React.useState("");
  const [boundPath, setBoundPath] = React.useState<string | null>(null);
  const [isPending, setIsPending] = React.useState(false);
  const [notice, setNotice] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  /** Newest seed request. An older answer that arrives late is discarded. */
  const seedGeneration = React.useRef(0);
  /** Set once the operator types or acts, so a late seed cannot overwrite. */
  const interacted = React.useRef(false);

  React.useEffect(() => {
    const generation = ++seedGeneration.current;
    interacted.current = false;
    setBoundPath(null);
    setPath("");
    setError(null);
    setNotice(null);
    void (async () => {
      try {
        const stored = await getPromptSource(definitionId);
        // Fenced by generation and by the operator: an answer for a definition
        // the dialog has moved off, or one that lost the race to typing or a
        // reload, must not land on top of newer state.
        if (generation !== seedGeneration.current || interacted.current) {
          return;
        }
        setBoundPath(stored);
        if (stored !== null) {
          setPath(stored);
        }
      } catch (caught) {
        if (generation !== seedGeneration.current || interacted.current) {
          return;
        }
        // A corrupt sidecar is reported, not read as "nothing is bound": the
        // operator would otherwise rebind over a mapping that is still there.
        setError(
          `Could not read this agent's instructions file setting: ${
            caught instanceof Error ? caught.message : String(caught)
          }`,
        );
      }
    })();
  }, [definitionId]);

  const busy = disabled || isPending;
  const reloadEnabled = canReloadPromptSource(path, busy);
  const clearEnabled = canClearPromptSource(busy);

  const run = async (nextPath: string | null) => {
    interacted.current = true;
    setIsPending(true);
    setError(null);
    setNotice(null);
    try {
      const result = await setPromptSourceAndReload(definitionId, nextPath);
      // `path` is set exactly when a mapping was stored, so it is also the
      // binding the field now shows: null after a clear, and null after a
      // reload whose sidecar write failed (`mappingError` says which).
      setBoundPath(result.path);
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
    // The path never leaves this machine and is never submitted with the
    // definition, so the marker keeps the parent dialog from counting a
    // keystroke here as an unsaved edit to the agent.
    <div className="space-y-1.5" {...{ [DIRTY_EXEMPT_ATTRIBUTE]: "" }}>
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
            onChange={(event) => {
              interacted.current = true;
              setPath(event.target.value);
            }}
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
        {error ?? notice ?? promptSourceHint(boundPath)}
      </p>
    </div>
  );
}
