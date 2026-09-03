import type { ReactNode } from "react";

/**
 * Presentation pieces for the design system pages only. These are documentation
 * furniture, not product components — the shared primitive layer gets built one
 * component at a time as the product repeats something.
 */

export function PageHeader({
  title,
  intro,
  status,
}: {
  title: string;
  intro: string;
  status?: string;
}) {
  return (
    <header className="mb-10 flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-3">
        <h1 className="text-3xl font-semibold text-primary">{title}</h1>
        {status ? <StatusPill>{status}</StatusPill> : null}
      </div>
      <p className="max-w-2xl text-sm leading-relaxed text-secondary">
        {intro}
      </p>
    </header>
  );
}

export function StatusPill({ children }: { children: ReactNode }) {
  return (
    <span className="rounded-full bg-warning-tint px-2.5 py-0.5 text-xs font-medium text-warning">
      {children}
    </span>
  );
}

export function Section({
  title,
  description,
  children,
}: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <section className="mb-12 flex flex-col gap-4">
      <div className="flex flex-col gap-1.5">
        <h2 className="text-lg font-semibold text-primary">{title}</h2>
        {description ? (
          <p className="max-w-2xl text-sm leading-relaxed text-secondary">
            {description}
          </p>
        ) : null}
      </div>
      {children}
    </section>
  );
}

export function Note({ children }: { children: ReactNode }) {
  return (
    <p className="max-w-2xl rounded-lg border border-secondary bg-inset px-4 py-3 text-xs leading-relaxed text-secondary">
      {children}
    </p>
  );
}

export function Stub({ what, decide }: { what: string; decide: string[] }) {
  return (
    <div className="flex max-w-2xl flex-col gap-3 rounded-lg border border-primary bg-inset px-5 py-4">
      <p className="text-sm text-secondary">{what}</p>
      <div className="flex flex-col gap-1.5">
        <p className="text-xs font-semibold uppercase tracking-wide text-tertiary">
          Still to decide
        </p>
        <ul className="flex list-disc flex-col gap-1 pl-4">
          {decide.map((item) => (
            <li key={item} className="text-xs text-secondary">
              {item}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

/** Renders a live swatch of whatever a CSS custom property currently holds. */
export function Swatch({
  variable,
  label,
  sublabel,
  translucent,
}: {
  variable: string;
  label: string;
  sublabel?: string;
  translucent?: boolean;
}) {
  return (
    <div className="flex min-w-0 flex-col gap-1.5">
      <div
        className={`h-16 rounded-lg border border-secondary ${
          translucent ? "blur-chrome" : ""
        }`}
        style={{ background: `var(${variable})` }}
      />
      <code className="truncate text-xs font-medium text-primary">{label}</code>
      {sublabel ? (
        <span className="text-xs leading-snug text-tertiary">{sublabel}</span>
      ) : null}
    </div>
  );
}
