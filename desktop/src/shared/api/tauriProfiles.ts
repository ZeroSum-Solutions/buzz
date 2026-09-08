import { invokeTauri } from "@/shared/api/tauri";
import type {
  Profile,
  UpdateProfileInput,
  UserProfileSummary,
  UserSearchPage,
  UserSearchResult,
  UsersBatchResponse,
} from "@/shared/api/types";

type RawProfile = {
  pubkey: string;
  display_name: string | null;
  avatar_url: string | null;
  about: string | null;
  nip05_handle: string | null;
  owner_pubkey: string | null;
  has_profile_event?: boolean;
};

type RawUserProfileSummary = Omit<RawProfile, "pubkey"> & {
  name?: string | null;
  is_agent?: boolean;
};

type RawUsersBatchResponse = {
  profiles: Record<string, RawUserProfileSummary>;
  missing: string[];
};

type RawUserSearchResult = RawUserProfileSummary & { pubkey: string };

type RawSearchUsersResponse = {
  users: RawUserSearchResult[];
  next_cursor?: string | null;
};

function fromRawProfile(profile: RawProfile): Profile {
  return {
    pubkey: profile.pubkey,
    displayName: profile.display_name,
    avatarUrl: profile.avatar_url,
    about: profile.about,
    nip05Handle: profile.nip05_handle,
    ownerPubkey: profile.owner_pubkey,
    hasProfileEvent: profile.has_profile_event ?? false,
  };
}

function fromRawUserProfileSummary(
  profile: RawUserProfileSummary,
): UserProfileSummary {
  return {
    displayName: profile.display_name,
    name: profile.name ?? null,
    avatarUrl: profile.avatar_url,
    about: profile.about,
    nip05Handle: profile.nip05_handle,
    ownerPubkey: profile.owner_pubkey,
    isAgent: profile.is_agent ?? false,
  };
}

function fromRawUserSearchResult(user: RawUserSearchResult): UserSearchResult {
  return {
    pubkey: user.pubkey,
    displayName: user.display_name,
    avatarUrl: user.avatar_url,
    about: user.about,
    nip05Handle: user.nip05_handle,
    ownerPubkey: user.owner_pubkey,
    isAgent: user.is_agent ?? false,
  };
}

export async function getProfile(): Promise<Profile> {
  const profile = await invokeTauri<RawProfile>("get_profile");
  return fromRawProfile(profile);
}

export async function updateProfile(
  input: UpdateProfileInput,
): Promise<Profile> {
  const profile = await invokeTauri<RawProfile>("update_profile", input);
  return fromRawProfile(profile);
}

export async function updateProfileAtRelay(input: {
  relayUrl: string;
  expectedPubkey: string;
  expectedAvatarUrl: string | null;
  avatarUrl: string;
}): Promise<Profile> {
  const profile = await invokeTauri<RawProfile>(
    "update_profile_at_relay",
    input,
  );
  return fromRawProfile(profile);
}

export async function getUserProfile(pubkey?: string): Promise<Profile> {
  const profile = await invokeTauri<RawProfile>("get_user_profile", { pubkey });
  return fromRawProfile(profile);
}

export async function getUsersBatch(
  pubkeys: string[],
): Promise<UsersBatchResponse> {
  const result: UsersBatchResponse = { profiles: {}, missing: [] };
  const uniquePubkeys = [...new Set(pubkeys)];
  // Historical file authors can exceed a single native request. Keep network
  // concurrency at one and fail the whole fetch if any partition fails.
  for (let offset = 0; offset < uniquePubkeys.length; offset += 256) {
    const response = await invokeTauri<RawUsersBatchResponse>(
      "get_users_batch",
      {
        pubkeys: uniquePubkeys.slice(offset, offset + 256),
      },
    );
    for (const [pubkey, profile] of Object.entries(response.profiles)) {
      result.profiles[pubkey] = fromRawUserProfileSummary(profile);
    }
    result.missing.push(...response.missing);
  }
  return result;
}

export async function searchUsers(
  query: string,
  limit = 8,
  cursor?: string | null,
): Promise<UserSearchPage> {
  const response = await invokeTauri<RawSearchUsersResponse>("search_users", {
    query,
    limit,
    cursor: cursor ?? null,
  });
  return {
    users: response.users.map(fromRawUserSearchResult),
    nextCursor: response.next_cursor ?? null,
  };
}
