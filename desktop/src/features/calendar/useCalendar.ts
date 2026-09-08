import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useIdentityQuery } from "@/shared/api/hooks";
import {
  connectCalendar,
  disconnectCalendar,
  getCalendarEvents,
  getCalendarStatus,
  abandonCalendarRevocation,
  clearCalendarRevocation,
} from "@/shared/api/tauriCalendar";

export function useCalendar() {
  const identity = useIdentityQuery();
  const queryClient = useQueryClient();
  const expectedIdentity = identity.data?.pubkey ?? "";
  const key = ["calendar", expectedIdentity];
  const enabled = !!identity.data?.pubkey && !identity.data.locked;
  const status = useQuery({
    queryKey: [...key, "status"],
    queryFn: () => getCalendarStatus(expectedIdentity),
    enabled,
    staleTime: 15_000,
    refetchInterval: 15_000,
  });
  const events = useQuery({
    queryKey: [...key, "events", status.data?.generation],
    queryFn: () => getCalendarEvents(expectedIdentity),
    enabled: enabled && status.data?.connected === true,
    staleTime: 60_000,
    gcTime: 0,
    refetchInterval: 60_000,
    retry: false,
  });
  const refresh = () => queryClient.invalidateQueries({ queryKey: key });
  const connect = useMutation({
    mutationFn: () => connectCalendar(expectedIdentity),
    onSuccess: refresh,
  });
  const disconnect = useMutation({
    mutationFn: (generation: number) =>
      disconnectCalendar(expectedIdentity, generation),
    onSuccess: async () => {
      queryClient.removeQueries({ queryKey: [...key, "events"] });
      await refresh();
    },
  });
  const abandon = useMutation({
    mutationFn: (generation: number) =>
      abandonCalendarRevocation(expectedIdentity, generation),
    onSuccess: refresh,
  });
  const clear = useMutation({
    mutationFn: (generation: number) =>
      clearCalendarRevocation(expectedIdentity, generation),
    onSuccess: refresh,
  });
  return {
    status,
    events,
    connect,
    disconnect,
    abandon,
    clear,
    refresh,
    expectedIdentity,
  };
}
