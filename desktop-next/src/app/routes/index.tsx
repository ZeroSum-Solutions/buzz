import { Link, createFileRoute } from "@tanstack/react-router";

function Home() {
  return (
    <div className="flex min-h-screen flex-col items-center justify-center gap-4 px-6 text-center">
      <h1 className="text-2xl font-semibold text-primary">Buzz</h1>
      <p className="max-w-md text-sm text-secondary">
        The new client. Nothing here yet — the shell and its capabilities come
        later. The design system is the foundation being built first.
      </p>
      <Link
        to="/design"
        className="rounded-lg bg-accent px-4 py-2 text-sm font-medium text-on-accent transition-opacity hover:opacity-90"
      >
        Open the design system
      </Link>
    </div>
  );
}

export const Route = createFileRoute("/")({ component: Home });
