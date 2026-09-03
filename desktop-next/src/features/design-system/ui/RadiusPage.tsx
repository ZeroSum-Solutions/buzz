import { PageHeader, Section, Stub } from "./primitives";

export function RadiusPage() {
  return (
    <>
      <PageHeader
        title="Radius"
        status="not started"
        intro="The design exploration is consistently soft — rounded panels, pill-shaped chrome, gently rounded chips — but the specific values have not been pulled into a scale yet."
      />

      <Section title="What this page will hold">
        <Stub
          what="A radius scale, and the named roles that point at it: the panel corner, the control corner, the chip corner, and the fully-round pill."
          decide={[
            "How many steps the scale needs",
            "Which roles exist, and what each one is for",
            "Whether nested corners follow a rule, so a control inside a panel reads correctly",
          ]}
        />
      </Section>
    </>
  );
}
