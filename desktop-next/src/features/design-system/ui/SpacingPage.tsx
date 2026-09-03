import { PageHeader, Section, Stub } from "./primitives";

export function SpacingPage() {
  return (
    <>
      <PageHeader
        title="Spacing"
        status="not started"
        intro="Tailwind's default spacing scale is currently in play, unexamined. Whether that is the right rhythm for this design is an open decision."
      />

      <Section title="What this page will hold">
        <Stub
          what="A spacing scale with the same two-layer treatment as colour: raw steps below, and named roles for the spacings that carry meaning — the gutter between rows, the inset of a panel, the gap inside a control."
          decide={[
            "Whether the stock scale stays or the design needs its own rhythm",
            "Which spacings are named roles rather than raw steps",
            "Whether density varies per surface, and if so how that is expressed",
            "The relationship between spacing and the type ramp",
          ]}
        />
      </Section>
    </>
  );
}
