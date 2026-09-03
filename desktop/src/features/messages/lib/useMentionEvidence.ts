import * as React from "react";

/** A bounded verification window, not a directory polling loop. */
export function useMentionEvidence({
  scope,
  open,
  agentKeys,
  directoryUpdatedAt,
  directoryError,
  retry,
}: {
  scope: string;
  open: boolean;
  agentKeys: ReadonlySet<string>;
  directoryUpdatedAt: number;
  directoryError: boolean;
  retry: () => void;
}) {
  const known = React.useRef({ scope, keys: new Set<string>() });
  if (known.current.scope !== scope) known.current = { scope, keys: new Set() };
  for (const key of agentKeys) known.current.keys.add(key);
  const [attempt, setAttempt] = React.useState(0);
  const [expired, setExpired] = React.useState(false);
  const [now, setNow] = React.useState(Date.now);
  React.useEffect(() => {
    setExpired(false);
    if (!open || !scope || attempt < 0) return;
    const timer = setTimeout(() => setExpired(true), 5000);
    return () => clearTimeout(timer);
  }, [scope, open, attempt]);
  React.useEffect(() => {
    setNow(Date.now());
    const delay = directoryUpdatedAt + 180_000 - Date.now();
    if (delay <= 0) return;
    const timer = setTimeout(() => setNow(Date.now()), delay);
    return () => clearTimeout(timer);
  }, [directoryUpdatedAt]);
  const retryVerification = React.useCallback(() => {
    setExpired(false);
    setAttempt((value) => value + 1);
    retry();
  }, [retry]);
  return {
    knownAgentPubkeys: known.current.keys,
    verificationFailed: directoryError || expired,
    presenceFresh: directoryUpdatedAt > 0 && now - directoryUpdatedAt < 180_000,
    retryVerification,
  };
}
