import assert from "node:assert/strict";
import test from "node:test";

import {
  catalogBookkeepingSentence,
  personaSaveNotice,
} from "./personaSaveNotice.ts";

test("test_plain_save_notice_says_nothing_about_the_catalog", () => {
  const notice = personaSaveNotice("Helper", null);
  assert.equal(notice, "Updated Helper.");
  assert.ok(!/catalog/i.test(notice));
});

test("test_accepted_publish_notice_claims_the_catalog_has_the_edit", () => {
  assert.match(
    personaSaveNotice("Helper", "published"),
    /published it to the community catalog/,
  );
});

// The whole point of routing "Save and publish" through the strict command is
// that a queued edit must NOT be reported as published — the relay hasn't taken
// it yet, so the catalog still shows the old definition.
test("test_queued_publish_notice_does_not_claim_the_edit_is_published", () => {
  const notice = personaSaveNotice("Helper", "queued");
  assert.match(notice, /queued/);
  assert.ok(
    !/\bpublished\b/.test(notice),
    "a queued edit must not be described as published",
  );
});

// The relay took the head; only the local sync record did not update, so the
// flush loop will send it again. Saying nothing about that leaves the user
// watching a change publish itself twice with no explanation.
test("test_save_notice_reports_a_bookkeeping_failure_after_a_published_head", () => {
  const notice = personaSaveNotice(
    "Helper",
    "published",
    "retention db is locked",
  );
  assert.match(notice, /published it to the community catalog/);
  assert.match(notice, /local sync record did not update/);
  assert.match(notice, /retention db is locked/);
});

test("test_save_notice_stays_silent_when_the_bookkeeping_succeeded", () => {
  assert.ok(
    !/sync record/.test(personaSaveNotice("Helper", "published", null)),
    "a clean publish must not mention a failure that did not happen",
  );
  assert.equal(catalogBookkeepingSentence(null), "");
  assert.equal(catalogBookkeepingSentence(undefined), "");
});
