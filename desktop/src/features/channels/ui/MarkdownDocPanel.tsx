import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Download, FileText, Loader2, Printer } from "lucide-react";
import { toast } from "sonner";

import { invokeTauri } from "@/shared/api/tauri";
import {
  fetchMarkdownDocBytes,
  isMediaTooLargeError,
} from "@/shared/api/tauriMedia";
import {
  focusMarkdownDocPanelClose,
  restoreFocusToMarkdownDocOpener,
} from "@/features/channels/ui/markdownDocFocus";
import { exportMarkdownDocumentToPdf } from "@/features/channels/ui/markdownDocPdfExport";
import { useEscapeKey } from "@/shared/hooks/useEscapeKey";
import { useIsThreadPanelOverlay } from "@/shared/hooks/use-mobile";
import {
  AuxiliaryPanel,
  AuxiliaryPanelBody,
  AuxiliaryPanelHeader,
  AuxiliaryPanelHeaderActions,
  AuxiliaryPanelHeaderGroup,
  AuxiliaryPanelHeaderTitleBlock,
} from "@/shared/layout/AuxiliaryPanel";
import { Button } from "@/shared/ui/button";
import { Markdown, SyntaxHighlightedCode } from "@/shared/ui/markdown";
import { isMarkdownDocumentTooComplex } from "@/shared/ui/markdown/documentMode";
import {
  decodeMarkdownDocBytes,
  type MarkdownDocDecodeResult,
} from "@/shared/ui/markdown/markdownDocFile";
import { parseLocalMarkdownDocUrl } from "@/shared/ui/markdown/pathLinks";
import { SegmentedControl } from "@/shared/ui/segmented-control";

type MarkdownDocView = "preview" | "code";

type MarkdownDocPanelProps = {
  /**
   * Raw relay `/media/` URL of the attachment, or a `buzz-local-file:` target
   * naming a local document resolved by the path-link resolver.
   */
  url: string;
  /** Human-readable filename from the imeta `filename` field. */
  filename: string;
  isSinglePanelView?: boolean;
  layout?: "standalone" | "split";
  onClose: () => void;
  transparentChrome?: boolean;
  widthPx: number;
};

const VIEW_OPTIONS = [
  { value: "preview", label: "Preview" },
  { value: "code", label: "Code" },
] as const;

function decodeErrorMessage(kind: "too-large" | "binary"): string {
  return kind === "too-large"
    ? "This file is too large to preview."
    : "This file isn't valid text, so it can't be previewed.";
}

const PREVIEW_TOO_COMPLEX_MESSAGE =
  "This document has too many elements to render a formatted preview. Switch to Code view, or download it.";

/**
 * Right auxiliary panel rendering a shared markdown attachment in-app.
 *
 * Relay media URLs require relay auth (plain browser requests 401), so the
 * content is fetched through the authenticated `fetch_markdown_doc_bytes`
 * Tauri command — which enforces the viewer's 2 MiB cap natively during the
 * fetch — and rendered with the same markdown pipeline chat messages use.
 * The Preview/Code toggle switches between rendered markdown and the
 * syntax-highlighted source.
 */
export function MarkdownDocPanel({
  url,
  filename,
  isSinglePanelView = false,
  layout = "standalone",
  onClose,
  transparentChrome = false,
  widthPx,
}: MarkdownDocPanelProps) {
  const isOverlay = useIsThreadPanelOverlay();
  useEscapeKey(onClose, isOverlay || isSinglePanelView);
  const [view, setView] = React.useState<MarkdownDocView>("preview");
  // A local document opened from a path link. `null` for a relay attachment,
  // which keeps the authenticated media fetch and the Download action.
  const localPath = React.useMemo(() => parseLocalMarkdownDocUrl(url), [url]);

  // Opening can unmount the section holding the focused attachment card
  // (narrow layout swaps the whole channel out), and closing unmounts this
  // panel — move focus in on mount and hand it back to the opener card on
  // unmount so keyboard users never fall to <body>.
  React.useEffect(() => {
    const cancel = focusMarkdownDocPanelClose();
    return () => {
      cancel();
      restoreFocusToMarkdownDocOpener(url);
    };
  }, [url]);

  // Blob URLs are content-addressed (`/media/{sha256}.{ext}`), so a fetched
  // document never changes under its URL — cache it for the session.
  const docQuery = useQuery<MarkdownDocDecodeResult>({
    queryKey: ["markdown-doc", url],
    queryFn: async ({ signal }) => {
      // A local document is read through `read_path_link_markdown`, which
      // re-resolves the path against the allowed roots and enforces the same
      // 2 MiB cap natively — the panel never reads a path it was handed.
      if (localPath !== null) {
        return {
          kind: "ok",
          text: await invokeTauri<string>("read_path_link_markdown", {
            candidate: localPath,
            senderPubkey: null,
          }),
        };
      }
      try {
        return decodeMarkdownDocBytes(await fetchMarkdownDocBytes(url, signal));
      } catch (err) {
        // The native 2 MiB cap refuses oversized documents during the fetch
        // (the in-frontend decode check never sees their bytes). Surface it
        // as the too-large fallback rather than a generic fetch failure.
        if (isMediaTooLargeError(err)) return { kind: "too-large" };
        throw err;
      }
    },
    staleTime: Number.POSITIVE_INFINITY,
    retry: 1,
  });

  // A local document is already on this Mac: the same action hands it to the
  // OS default handler instead of downloading it from the relay. Either way
  // the failure is surfaced, never swallowed.
  const handleDownload = React.useCallback(() => {
    const command = localPath !== null ? "open_path_link" : "download_file";
    const args =
      localPath !== null
        ? { candidate: localPath, senderPubkey: null }
        : { url, filename };
    invokeTauri(command, args).catch((err: unknown) => {
      const fallback = localPath !== null ? "Open failed" : "Download failed";
      toast.error(err instanceof Error ? err.message : fallback);
    });
  }, [filename, localPath, url]);
  const primaryActionLabel =
    localPath !== null ? `Open ${filename}` : `Download ${filename}`;
  const primaryActionText = localPath !== null ? "Open file" : "Download file";

  const decoded = docQuery.data;
  const [isExportingPdf, setIsExportingPdf] = React.useState(false);

  // Bounds the render by the parser's own work and then by the parsed tree's
  // node count, independent of the byte cap above: a flat list of one-line
  // items, one line packed with delimiters, or a shallow-looking document held
  // at depth across thousands of lines all cost far more than their size
  // suggests (see markdownDocFile.ts and markdownParseBudget.ts). Preview and
  // Export run the same parse, so one predicate gates both — a document too
  // complex to preview is too complex to print. Memoized because the second
  // half of that predicate *is* a parse; it must not run on every render.
  // Code view is safe without this gate — SyntaxHighlightedCode bounds its own
  // highlighting and plain-text fallback independently, so it stays available.
  const previewTooComplex = React.useMemo(
    () => decoded?.kind === "ok" && isMarkdownDocumentTooComplex(decoded.text),
    [decoded],
  );

  // Exportable only when the document both decoded and is inside that bound,
  // so the button is never offered for a document the panel itself refuses to
  // render.
  const documentText =
    decoded?.kind === "ok" && !previewTooComplex ? decoded.text : null;

  // Export renders the document in document mode (links kept, attachments as
  // links, code never collapsed) and prints it through the Rust exporter.
  // A cancelled save dialog resolves false and is not an error; every other
  // failure is surfaced, never swallowed.
  const handleExportPdf = React.useCallback(() => {
    if (documentText === null) return;
    setIsExportingPdf(true);
    exportMarkdownDocumentToPdf({ content: documentText, filename })
      .then((saved) => {
        if (saved) toast.success(`Exported ${filename} as PDF`);
      })
      .catch((err: unknown) => {
        toast.error(err instanceof Error ? err.message : "PDF export failed");
      })
      .finally(() => setIsExportingPdf(false));
  }, [documentText, filename]);

  const errorMessage = docQuery.isError
    ? localPath !== null
      ? docQuery.error instanceof Error && docQuery.error.message
        ? docQuery.error.message
        : "Couldn't open this file."
      : "Couldn't load this file from the relay."
    : decoded && decoded.kind !== "ok"
      ? decodeErrorMessage(decoded.kind)
      : null;

  return (
    <AuxiliaryPanel
      isSinglePanelView={isSinglePanelView}
      layout={layout}
      onClose={onClose}
      testId="markdown-doc-panel"
      transparentChrome={transparentChrome}
      widthPx={widthPx}
      header={
        <AuxiliaryPanelHeader
          backdrop={layout !== "split" && !isOverlay}
          backdropSurface="soft"
          inset={layout !== "split" ? "wide" : "default"}
        >
          <AuxiliaryPanelHeaderGroup>
            <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
            <AuxiliaryPanelHeaderTitleBlock title={filename} />
          </AuxiliaryPanelHeaderGroup>
          <AuxiliaryPanelHeaderActions includeCloseAction>
            {documentText !== null ? (
              <Button
                aria-label={`Export ${filename} as PDF`}
                data-testid="markdown-doc-export-pdf"
                disabled={isExportingPdf}
                onClick={handleExportPdf}
                size="icon"
                variant="ghost"
              >
                {isExportingPdf ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <Printer className="h-4 w-4" />
                )}
              </Button>
            ) : null}
            <Button
              aria-label={primaryActionLabel}
              data-testid="markdown-doc-download"
              onClick={handleDownload}
              size="icon"
              variant="ghost"
            >
              <Download className="h-4 w-4" />
            </Button>
          </AuxiliaryPanelHeaderActions>
        </AuxiliaryPanelHeader>
      }
    >
      <AuxiliaryPanelBody className="flex min-h-0 flex-col" panelPadding>
        {/* The view picker gets its own pinned row below the title: sharing
            the title row squeezed the filename out, and the header chrome
            band overlays anything placed directly after it in the header
            slot — so the row lives inside the chrome-padded body instead. */}
        {decoded?.kind === "ok" ? (
          <div className="flex shrink-0 items-center px-4 pb-2">
            <SegmentedControl
              legend="Document view"
              onValueChange={setView}
              optionTestIdPrefix="markdown-doc-view"
              options={VIEW_OPTIONS}
              size="compact"
              testId="markdown-doc-view-toggle"
              value={view}
            />
          </div>
        ) : null}
        <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-6">
          {docQuery.isPending ? (
            <div
              className="flex items-center justify-center py-12"
              data-testid="markdown-doc-loading"
            >
              <Loader2 className="h-5 w-5 animate-spin text-muted-foreground/70" />
            </div>
          ) : errorMessage !== null ? (
            <div className="flex flex-col items-center gap-3 py-12 text-center">
              <p className="text-sm text-muted-foreground">{errorMessage}</p>
              <Button onClick={handleDownload} size="sm" variant="secondary">
                <Download className="mr-1.5 h-4 w-4" />
                {primaryActionText}
              </Button>
            </div>
          ) : decoded?.kind === "ok" ? (
            view === "preview" ? (
              previewTooComplex ? (
                <div
                  className="flex flex-col items-center gap-3 py-12 text-center"
                  data-testid="markdown-doc-preview-too-complex"
                >
                  <p className="text-sm text-muted-foreground">
                    {PREVIEW_TOO_COMPLEX_MESSAGE}
                  </p>
                  <Button
                    onClick={handleDownload}
                    size="sm"
                    variant="secondary"
                  >
                    <Download className="mr-1.5 h-4 w-4" />
                    {primaryActionText}
                  </Button>
                </div>
              ) : (
                <Markdown
                  blockCode
                  className="pt-3 text-sm"
                  content={decoded.text}
                  hardLineBreaks={false}
                />
              )
            ) : (
              <pre
                className="overflow-x-auto pt-3 text-xs leading-relaxed"
                data-testid="markdown-doc-code"
              >
                {/* Shiki's synchronous-tokenization guard caps highlighting at
                  150 lines; longer documents render as plain text here. */}
                <SyntaxHighlightedCode
                  code={decoded.text}
                  language="markdown"
                />
              </pre>
            )
          ) : null}
        </div>
      </AuxiliaryPanelBody>
    </AuxiliaryPanel>
  );
}
