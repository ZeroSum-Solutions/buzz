import { Link, Outlet } from "@tanstack/react-router";
import type { ReactNode } from "react";

import { useColorScheme } from "@/shared/theme/useColorScheme";

const SECTIONS: Array<{ heading: string; items: Array<[string, string]> }> = [
  {
    heading: "Foundations",
    items: [
      ["Colour", "/design/colour"],
      ["Typography", "/design/typography"],
      ["Spacing", "/design/spacing"],
      ["Radius", "/design/radius"],
      ["Elevation", "/design/elevation"],
      ["Glass", "/design/glass"],
      ["Motion", "/design/motion"],
    ],
  },
  {
    heading: "System",
    items: [
      ["Vocabulary", "/design/vocabulary"],
      ["Growing the system", "/design/growth"],
    ],
  },
  {
    heading: "Components",
    items: [],
  },
];

function NavLink({ to, children }: { to: string; children: ReactNode }) {
  return (
    <Link
      to={to}
      className="block rounded-lg px-3 py-2 text-label text-secondary transition-colors hover:bg-hover hover:text-primary"
      activeProps={{
        className:
          "block rounded-lg px-3 py-2 text-label bg-accent-tint text-accent",
      }}
    >
      {children}
    </Link>
  );
}

export function DesignSystemLayout() {
  const { scheme, toggle } = useColorScheme();

  return (
    /* Narrow: the nav stacks above the content as a wrapped list, because a
       256px column beside a reading column leaves neither enough room. From lg
       it becomes the sticky side rail. */
    <div className="flex min-h-screen flex-col bg-panel lg:flex-row">
      <nav
        aria-label="Design system"
        className="flex shrink-0 flex-col gap-8 px-4 py-8 lg:sticky lg:top-0 lg:h-screen lg:w-64 lg:overflow-y-auto"
      >
        <div className="px-3">
          <Link to="/design" className="text-label text-primary">
            Buzz Design System
          </Link>
          <p className="mt-1 text-caption text-tertiary">
            Rendered from the tokens themselves
          </p>
        </div>

        <div className="flex flex-1 flex-col gap-6 lg:gap-7">
          {SECTIONS.map((section) => (
            <div key={section.heading} className="flex flex-col gap-1">
              <h2 className="px-3 pb-1.5 text-meta text-tertiary">
                {section.heading}
              </h2>
              {section.items.length === 0 ? (
                <p className="max-w-prose px-3 py-1 text-caption text-tertiary">
                  None yet — the primitive layer gets built one component at a
                  time, as the product repeats something.
                </p>
              ) : (
                /* Narrow: links wrap as a row so the nav costs a few lines
                   instead of a screen. From lg they return to a column. */
                <div className="flex flex-wrap gap-1 lg:flex-col">
                  {section.items.map(([label, to]) => (
                    <NavLink key={to} to={to}>
                      {label}
                    </NavLink>
                  ))}
                </div>
              )}
            </div>
          ))}
        </div>

        <button
          type="button"
          onClick={toggle}
          aria-label={`Switch to ${scheme === "light" ? "dark" : "light"} mode`}
          className="mx-3 self-start rounded-lg bg-inset px-3 py-2 text-label text-secondary transition-colors hover:bg-hover hover:text-primary"
        >
          {scheme === "light" ? "Dark mode" : "Light mode"}
        </button>
      </nav>

      <main className="min-w-0 flex-1 px-6 py-10 sm:px-10 lg:px-16">
        <div className="mx-auto max-w-3xl">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
