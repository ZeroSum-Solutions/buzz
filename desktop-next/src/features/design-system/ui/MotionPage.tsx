import { PageHeader, Section, Stub } from "./primitives";

export function MotionPage() {
  return (
    <>
      <PageHeader
        title="Motion"
        status="not started"
        intro="No durations or easings are defined yet. The rules that govern them already exist in DESIGN.md, which is the harder half."
      />

      <Section title="What this page will hold">
        <Stub
          what="Duration and easing tokens, and the named roles that pair them: a state change, an element entering, an overlay opening, a drag settling."
          decide={[
            "The duration steps, and which is the default",
            "Easing curves, and which motions get a spring rather than a curve",
            "How reduced-motion preferences are honoured at the token level rather than per component",
          ]}
        />
      </Section>

      <Section
        title="Rules that already apply"
        description="These are in DESIGN.md and do not wait for the tokens."
      >
        <ul className="flex list-disc flex-col gap-2 pl-5">
          {[
            "Direct manipulation follows the pointer exactly, with no easing — smoothing during a drag reads as lag. Spring physics belongs to what happens after release.",
            "Never animate blur. Re-blurring a large surface every frame is expensive enough to feel. Animate opacity instead.",
            "Motion explains a change; it does not decorate one. If removing an animation loses no information, remove it.",
          ].map((rule) => (
            <li key={rule} className="text-sm leading-relaxed text-secondary">
              {rule}
            </li>
          ))}
        </ul>
      </Section>
    </>
  );
}
