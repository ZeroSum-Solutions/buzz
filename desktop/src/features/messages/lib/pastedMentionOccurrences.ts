import { Extension } from "@tiptap/core";
import {
  Plugin,
  PluginKey,
  type EditorState,
  type Transaction,
} from "@tiptap/pm/state";

/**
 * Ownership of the text one paste inserted, tracked across later edits.
 *
 * A pasted mention's identity can only be bound once trusted state vouches for
 * it, and that check can need a relay round trip — so the answer lands well
 * after the insertion. "Is this label visible in the composer?" is not a fence
 * for that: the label the user deleted and then hand-typed reads the same as
 * the one this paste put there, and binding the former hands a stranger's
 * pubkey to text the composer's own candidates should have resolved.
 *
 * The plugin holds one range per in-flight paste and maps it through every
 * transaction, so settlement can ask the narrower question: does the label
 * still appear in the text *this* paste owns? A range dies when its content is
 * deleted or replaced, which is the fail-closed direction — a settlement with
 * no live range binds nothing.
 */
export const pastedMentionOccurrencesKey = new PluginKey<PastedMentionRanges>(
  "pastedMentionOccurrences",
);

type PastedMentionRange = { from: number; to: number };

type PastedMentionRanges = ReadonlyMap<number, PastedMentionRange>;

type PastedMentionOccurrenceCommand =
  | { type: "track"; id: number; from: number; to: number }
  | { type: "release"; id: number };

/**
 * The slice of `EditorView` these helpers use.
 *
 * `EditorView` satisfies it structurally, so production passes the real view
 * while a test can drive the real plugin over an `EditorState` without a DOM.
 */
export type PastedMentionOccurrenceView = {
  state: EditorState;
  dispatch: (tr: Transaction) => void;
  isDestroyed?: boolean;
};

/**
 * Every paste releases its range at settlement, so this cap is a backstop
 * rather than a working limit: a composer whose settlements somehow stop
 * arriving must not accumulate ranges for the life of the session.
 */
const MAX_TRACKED_OCCURRENCES = 50;

let nextOccurrenceId = 1;

/**
 * Map every tracked range through one transaction, dropping the dead ones.
 *
 * `from` maps with assoc 1 and `to` with assoc −1, so text typed at either
 * edge lands *outside* the range: an occurrence owns what the paste inserted
 * and nothing the user added around it. A range whose endpoint was deleted —
 * the select-all-and-retype case, and any edit that eats into the paste's head
 * or tail — is dropped rather than repaired.
 */
function remapPastedMentionRanges(
  current: PastedMentionRanges,
  tr: Transaction,
): PastedMentionRanges {
  const next = new Map<number, PastedMentionRange>();
  for (const [id, range] of current) {
    const from = tr.mapping.mapResult(range.from, 1);
    const to = tr.mapping.mapResult(range.to, -1);
    if (from.deleted || to.deleted || to.pos <= from.pos) continue;
    next.set(id, { from: from.pos, to: to.pos });
  }
  return next;
}

function applyPastedMentionCommand(
  current: PastedMentionRanges,
  command: PastedMentionOccurrenceCommand,
): PastedMentionRanges {
  const next = new Map(current);
  if (command.type === "release") {
    next.delete(command.id);
    return next;
  }
  next.set(command.id, { from: command.from, to: command.to });
  for (const id of next.keys()) {
    if (next.size <= MAX_TRACKED_OCCURRENCES) break;
    next.delete(id);
  }
  return next;
}

/**
 * Tracks the document range each in-flight pasted mention occupies.
 *
 * Registered in the shared composer extension list, so every composer that
 * accepts a mention paste — channel, DM, thread, edit, forum — can fence a
 * late identity binding to the text its paste still owns.
 */
export const PastedMentionOccurrencesExtension = Extension.create({
  name: "pastedMentionOccurrences",

  addProseMirrorPlugins() {
    return [
      new Plugin<PastedMentionRanges>({
        key: pastedMentionOccurrencesKey,
        state: {
          init: () => new Map(),
          apply(tr, current) {
            const mapped = tr.docChanged
              ? remapPastedMentionRanges(current, tr)
              : current;
            const command = tr.getMeta(pastedMentionOccurrencesKey) as
              | PastedMentionOccurrenceCommand
              | undefined;
            return command
              ? applyPastedMentionCommand(mapped, command)
              : mapped;
          },
        },
      }),
    ];
  },
});

/**
 * Start tracking `[from, to)` as one paste's own text.
 *
 * Returns `null` when there is nothing to own (an empty insertion) or when the
 * composer carries no occurrence plugin — callers treat that as "no live
 * occurrence" and bind nothing, so an unregistered extension costs a pasted
 * identity rather than binding one no fence can retire.
 */
export function trackPastedMentionOccurrence(
  view: PastedMentionOccurrenceView,
  from: number,
  to: number,
): number | null {
  if (view.isDestroyed || to <= from) return null;
  if (!pastedMentionOccurrencesKey.getState(view.state)) return null;
  const id = nextOccurrenceId++;
  view.dispatch(
    view.state.tr.setMeta(pastedMentionOccurrencesKey, {
      type: "track",
      id,
      from,
      to,
    }),
  );
  return id;
}

/** The text an occurrence still owns, or `null` once its range is gone. */
export function readPastedMentionOccurrenceText(
  view: PastedMentionOccurrenceView,
  id: number | null,
): string | null {
  if (id === null || view.isDestroyed) return null;
  const range = pastedMentionOccurrencesKey.getState(view.state)?.get(id);
  if (!range) return null;
  const { doc } = view.state;
  if (range.to > doc.content.size) return null;
  return doc.textBetween(range.from, range.to, "\n", "\n");
}

/** Stop tracking an occurrence whose paste has finished settling. */
export function releasePastedMentionOccurrence(
  view: PastedMentionOccurrenceView,
  id: number | null,
): void {
  if (id === null || view.isDestroyed) return;
  if (!pastedMentionOccurrencesKey.getState(view.state)?.has(id)) return;
  view.dispatch(
    view.state.tr.setMeta(pastedMentionOccurrencesKey, { type: "release", id }),
  );
}
