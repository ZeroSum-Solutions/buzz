import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Download, FileText, Loader2 } from "lucide-react";
import { toast } from "sonner";

import { invokeTauri } from "@/shared/api/tauri";
import { fetchMediaBytes } from "@/shared/api/tauriMedia";
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
import {
  decodeMarkdownDocBytes,
  type MarkdownDocDecodeResult,
} from "@/shared/ui/markdown/markdownDocFile";
import { SegmentedControl } from "@/shared/ui/segmented-control";

type MarkdownDocView = "preview" | "code";

type MarkdownDocPanelProps = {
  /** Raw relay `/media/` URL of the attachment. */
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

/**
 * Right auxiliary panel rendering a shared markdown attachment in-app.
 *
 * Relay media URLs require relay auth (plain browser requests 401), so the
 * content is fetched through the authenticated `fetch_media_bytes` Tauri
 * command and rendered with the same markdown pipeline chat messages use.
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

  // Blob URLs are content-addressed (`/media/{sha256}.{ext}`), so a fetched
  // document never changes under its URL — cache it for the session.
  const docQuery = useQuery<MarkdownDocDecodeResult>({
    queryKey: ["markdown-doc", url],
    queryFn: async () => decodeMarkdownDocBytes(await fetchMediaBytes(url)),
    staleTime: Number.POSITIVE_INFINITY,
    retry: 1,
  });

  const handleDownload = React.useCallback(() => {
    invokeTauri("download_file", { url, filename }).catch((err: unknown) => {
      const msg = err instanceof Error ? err.message : "Download failed";
      toast.error(msg);
    });
  }, [url, filename]);

  const decoded = docQuery.data;
  const errorMessage = docQuery.isError
    ? "Couldn't load this file from the relay."
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
          <AuxiliaryPanelHeaderGroup align="start">
            <FileText className="h-4 w-4 shrink-0 text-muted-foreground" />
            <AuxiliaryPanelHeaderTitleBlock title={filename} />
          </AuxiliaryPanelHeaderGroup>
          <AuxiliaryPanelHeaderActions includeCloseAction>
            {decoded?.kind === "ok" ? (
              <SegmentedControl
                legend="Document view"
                onValueChange={setView}
                optionTestIdPrefix="markdown-doc-view"
                options={VIEW_OPTIONS}
                size="compact"
                testId="markdown-doc-view-toggle"
                value={view}
              />
            ) : null}
            <Button
              aria-label={`Download ${filename}`}
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
      <AuxiliaryPanelBody className="overflow-y-auto px-4 pb-6" panelPadding>
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
              Download file
            </Button>
          </div>
        ) : decoded?.kind === "ok" ? (
          view === "preview" ? (
            <Markdown
              blockCode
              className="pt-3 text-sm"
              content={decoded.text}
              hardLineBreaks={false}
            />
          ) : (
            <pre
              className="overflow-x-auto pt-3 text-xs leading-relaxed"
              data-testid="markdown-doc-code"
            >
              {/* Shiki's synchronous-tokenization guard caps highlighting at
                  150 lines; longer documents render as plain text here. */}
              <SyntaxHighlightedCode code={decoded.text} language="markdown" />
            </pre>
          )
        ) : null}
      </AuxiliaryPanelBody>
    </AuxiliaryPanel>
  );
}
