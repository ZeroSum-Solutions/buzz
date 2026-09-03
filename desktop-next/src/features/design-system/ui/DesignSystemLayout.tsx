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
      className="block rounded-md px-3 py-1.5 text-sm text-secondary transition-colors hover:bg-hover hover:text-primary"
      activeProps={{
        className:
          "block rounded-md px-3 py-1.5 text-sm bg-accent-tint text-primary font-medium",
      }}
    >
      {children}
    </Link>
  );
}

export function DesignSystemLayout() {
  const { scheme, toggle } = useColorScheme();

  return (
    <div className="flex min-h-screen">
      <nav
        aria-label="Design system"
        className="sticky top-0 flex h-screen w-60 shrink-0 flex-col gap-6 overflow-y-auto border-r border-secondary bg-panel px-4 py-6"
      >
        <div className="px-3">
          <Link to="/design" className="text-sm font-semibold text-primary">
            Buzz Design System
          </Link>
          <p className="mt-1 text-xs text-tertiary">
            Rendered from the tokens themselves
          </p>
        </div>

        <div className="flex flex-1 flex-col gap-5">
          {SECTIONS.map((section) => (
            <div key={section.heading} className="flex flex-col gap-0.5">
              <h2 className="px-3 pb-1 text-xs font-semibold uppercase tracking-wide text-tertiary">
                {section.heading}
              </h2>
              {section.items.length === 0 ? (
                <p className="px-3 py-1 text-xs text-tertiary">
                  None yet — the primitive layer gets built one component at a
                  time, as the product repeats something.
                </p>
              ) : (
                section.items.map(([label, to]) => (
                  <NavLink key={to} to={to}>
                    {label}
                  </NavLink>
                ))
              )}
            </div>
          ))}
        </div>

        <button
          type="button"
          onClick={toggle}
          aria-label={`Switch to ${scheme === "light" ? "dark" : "light"} mode`}
          className="mx-3 rounded-md border border-primary bg-panel px-3 py-1.5 text-xs text-secondary transition-colors hover:bg-hover hover:text-primary"
        >
          {scheme === "light" ? "Dark mode" : "Light mode"}
        </button>
      </nav>

      <main className="min-w-0 flex-1 px-10 py-10">
        <div className="mx-auto max-w-4xl">
          <Outlet />
        </div>
      </main>
    </div>
  );
}
