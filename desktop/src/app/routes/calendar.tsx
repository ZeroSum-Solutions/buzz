import * as React from "react";
import { createFileRoute } from "@tanstack/react-router";

const CalendarScreen = React.lazy(async () => ({
  default: (await import("@/features/calendar/CalendarScreen")).CalendarScreen,
}));
export const Route = createFileRoute("/calendar")({
  component: () => (
    <React.Suspense fallback={<p role="status">Loading calendar…</p>}>
      <CalendarScreen />
    </React.Suspense>
  ),
});
