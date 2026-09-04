import * as React from "react";

import { useSetPromptSourceMutation } from "@/features/agents/promptSourceMutation";
import {
  getPromptSource,
  type PromptSourceBinding,
  resetPromptSources,
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
  canResetPromptSources,
  DIRTY_EXEMPT_ATTRIBUTE,
  PROMPT_SOURCE_RESET_WARNING,
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
 * operator has to retype an absolute path into — and says so only while the
 * binding still matches the agent's instructions.
 *
 * **Every answer that arrives late is fenced.** The dialog is persistent: it
 * keeps this component mounted across agents and re-mounts it on the next open,
 * while a reload started on one agent is still in flight. An answer is applied
 * only if this instance is still mounted and still on the definition that asked
 * for it; otherwise it is dropped, because `onPromptReloaded` writes straight
 * into the dialog's shared instructions field and would otherwise put one
 * agent's prompt into another agent's unsaved draft, where Save persists it.
 */
export function PromptSourceField({
  definitionId,
  disabled,
  onPromptReloaded,
}: PromptSourceFieldProps) {
  const [path, setPath] = React.useState("");
  const [binding, setBinding] = React.useState<PromptSourceBinding | null>(
    null,
  );
  const [notice, setNotice] = React.useState<string | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [seedFailed, setSeedFailed] = React.useState(false);
  const reload = useSetPromptSourceMutation();
  const [isResetting, setIsResetting] = React.useState(false);
  /**
   * Bumped whenever the field starts over on a definition. Every awaited
   * answer captures it and compares on return, so an older request cannot
   * write over newer state.
   */
  const generation = React.useRef(0);
  /** The definition this instance is showing right now. */
  const currentDefinitionId = React.useRef(definitionId);
  /**
   * False once this instance is gone. Refs outlive the unmount inside a
   * closure, so a request in flight when the dialog closes still finds this
   * and drops its answer instead of writing into whatever opened next.
   */
  const isMounted = React.useRef(true);

  /**
   * Whether an answer captured at `startedAt`, for `requestedDefinitionId`, may
   * still be applied. Both halves matter: the generation catches a restart on
   * the same definition, and the id catches a dialog that moved to another
   * agent without this component unmounting.
   */
  const isCurrent = React.useCallback(
    (startedAt: number, requestedDefinitionId: string): boolean =>
      isMounted.current &&
      startedAt === generation.current &&
      requestedDefinitionId === currentDefinitionId.current,
    [],
  );

  React.useEffect(() => {
    isMounted.current = true;
    return () => {
      isMounted.current = false;
    };
  }, []);

  React.useEffect(() => {
    const seedGeneration = ++generation.current;
    currentDefinitionId.current = definitionId;
    setBinding(null);
    setPath("");
    setError(null);
    setNotice(null);
    setSeedFailed(false);
    void (async () => {
      try {
        const stored = await getPromptSource(definitionId);
        if (!isCurrent(seedGeneration, definitionId)) {
          return;
        }
        setBinding(stored);
        if (stored !== null) {
          setPath(stored.path);
        }
      } catch (caught) {
        if (!isCurrent(seedGeneration, definitionId)) {
          return;
        }
        // A corrupt sidecar is reported, not read as "nothing is bound": the
        // operator would otherwise rebind over a mapping that is still there.
        // It also arms the reset, which is the only control that can clear it.
        setSeedFailed(true);
        setError(
          `Could not read this agent's instructions file setting: ${
            caught instanceof Error ? caught.message : String(caught)
          }`,
        );
      }
    })();
  }, [definitionId, isCurrent]);

  const isPending = reload.isPending || isResetting;
  const busy = disabled || isPending;
  const reloadEnabled = canReloadPromptSource(path, busy);
  const clearEnabled = canClearPromptSource(busy);
  const resetEnabled = canResetPromptSources(seedFailed, busy);

  const run = async (nextPath: string | null) => {
    // A deliberate action starts a new round: any seed still in flight has lost.
    const startedAt = ++generation.current;
    const requestedDefinitionId = definitionId;
    setError(null);
    setNotice(null);
    try {
      const result = await reload.mutateAsync({
        definitionId: requestedDefinitionId,
        path: nextPath,
      });
      if (!isCurrent(startedAt, requestedDefinitionId)) {
        return;
      }
      // `binding` is what the sidecar holds now: null after a clear, and after
      // a failed write the entry that survived (`mappingError` says which).
      setBinding(result.binding);
      if (result.binding !== null) {
        setPath(result.binding.path);
      }
      if (result.prompt !== null) {
        onPromptReloaded(result.prompt);
      }
      setNotice(promptSourceStatusMessage(result));
    } catch (caught) {
      if (!isCurrent(startedAt, requestedDefinitionId)) {
        return;
      }
      // Surfaced, never swallowed: a refused path is the user's next action.
      setError(caught instanceof Error ? caught.message : String(caught));
    }
  };

  const runReset = async () => {
    const startedAt = ++generation.current;
    const requestedDefinitionId = definitionId;
    setIsResetting(true);
    setError(null);
    setNotice(null);
    try {
      const quarantined = await resetPromptSources();
      if (!isCurrent(startedAt, requestedDefinitionId)) {
        return;
      }
      setSeedFailed(false);
      setBinding(null);
      setNotice(
        `Instructions-file settings were reset. The unreadable file was kept at ${quarantined}.`,
      );
    } catch (caught) {
      if (!isCurrent(startedAt, requestedDefinitionId)) {
        return;
      }
      setError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      if (isMounted.current) {
        setIsResetting(false);
      }
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
              // Typing supersedes a seed still in flight: the operator's own
              // value must not be replaced by an answer they did not wait for.
              generation.current += 1;
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
        {error ?? notice ?? promptSourceHint(binding)}
      </p>
      {resetEnabled ? (
        <div className="flex flex-wrap items-center gap-2">
          <Button
            aria-describedby="persona-prompt-source-reset-warning"
            onClick={() => void runReset()}
            type="button"
            variant="ghost"
          >
            Reset instructions-file settings
          </Button>
          <span
            className="text-xs leading-5 text-muted-foreground"
            id="persona-prompt-source-reset-warning"
          >
            {PROMPT_SOURCE_RESET_WARNING}
          </span>
        </div>
      ) : null}
    </div>
  );
}
