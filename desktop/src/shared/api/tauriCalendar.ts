import { invokeTauri } from "@/shared/api/tauri";
import type {
  CalendarTime,
  DatedEvent,
} from "@/features/calendar/calendarDates";

type CalendarText = { value: string; truncated: boolean };
export type CalendarEvent = DatedEvent & {
  etag: string | null;
  recurring_event_id: string | null;
  summary: CalendarText;
  location: CalendarText;
  description: CalendarText;
  can_edit: boolean;
  can_delete: boolean;
};
export type CalendarStatus = {
  configured: boolean;
  connected: boolean;
  email: string | null;
  generation: number | null;
  pending_revocations: number;
  revocations?: {
    generation: number;
    state: string;
    purge_confirmed: boolean;
  }[];
  error: string | null;
};
export type CalendarSnapshot = {
  events: CalendarEvent[];
  interval: {
    start_ms: number;
    end_ms: number;
    coverage:
      | { coverage: "complete" }
      | { coverage: "truncated"; reason: string };
  } | null;
  stale: boolean;
  refreshed_at_ms: number | null;
  generation: number;
};
export type CalendarEventFields = {
  summary: string;
  location: string;
  description: string;
  start: CalendarTime;
  end: CalendarTime;
};
export const getCalendarStatus = (expectedIdentity: string) =>
  invokeTauri<CalendarStatus>("calendar_status", { expectedIdentity });
export const connectCalendar = (expectedIdentity: string) =>
  invokeTauri<CalendarStatus>("calendar_connect", { expectedIdentity });
export const disconnectCalendar = (
  expectedIdentity: string,
  expectedGeneration: number,
) =>
  invokeTauri<CalendarStatus>("calendar_disconnect", {
    expectedIdentity,
    expectedGeneration,
  });
export const getCalendarEvents = (expectedIdentity: string) =>
  invokeTauri<CalendarSnapshot>("calendar_events", { expectedIdentity });
export const abandonCalendarRevocation = (
  expectedIdentity: string,
  generation: number,
) =>
  invokeTauri<CalendarStatus>("calendar_abandon_revocation", {
    expectedIdentity,
    generation,
  });
export const clearCalendarRevocation = (
  expectedIdentity: string,
  generation: number,
) =>
  invokeTauri<CalendarStatus>("calendar_clear_revocation", {
    expectedIdentity,
    generation,
  });
export const createCalendarEvent = (input: {
  expectedIdentity: string;
  expectedGeneration: number;
  eventId: string;
  fields: CalendarEventFields;
}) => invokeTauri<CalendarEvent>("calendar_create", { input });
export const updateCalendarEvent = (input: {
  expectedIdentity: string;
  expectedGeneration: number;
  eventId: string;
  etag: string;
  fields: Partial<CalendarEventFields>;
}) => invokeTauri<CalendarEvent>("calendar_update", { input });
export const deleteCalendarEvent = (input: {
  expectedIdentity: string;
  expectedGeneration: number;
  eventId: string;
  etag: string;
}) => invokeTauri<void>("calendar_delete", { input });
