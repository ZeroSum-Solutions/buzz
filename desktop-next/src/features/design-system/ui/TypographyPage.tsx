import { PageHeader, Section, Stub } from "./primitives";

export function TypographyPage() {
  return (
    <>
      <PageHeader
        title="Typography"
        status="not started"
        intro="The next system to define. The starting point is the type stack every current Buzz client already shares — Inter for interface text, a monospace for code — and the decision to make is whether the redesign keeps it."
      />

      <Section title="What this page will hold">
        <Stub
          what="A named size ramp where each step says what it is for, plus weight, line height, letter spacing, and the semantic text roles that sit on top of them."
          decide={[
            "Whether Inter stays, or the redesign wants its own face",
            "The size ramp: how many steps, and what each one is for",
            "Which sizes are named roles (conversation text, metadata) rather than raw steps",
            "Weight and line-height pairings per step",
            "How the relative-unit contract is enforced, so text follows the person's font-size preference and keyboard zoom",
          ]}
        />
      </Section>

      <Section
        title="The one rule that already exists"
        description="Anything readable uses relative units. The current client learned this the hard way: hardcoded pixel sizes freeze against zoom, and a CI guard now rejects them. Whatever ramp this becomes, it inherits that contract."
      >
        <div className="rounded-lg border border-secondary bg-inset px-4 py-3">
          <p className="text-xs leading-relaxed text-secondary">
            The Cash Sans and BlockUI type variables that appear in the design
            exploration are contamination from another Figma library linked into
            that file. They exist nowhere in any Buzz codebase and need no
            cleanup — only a decision not to inherit them.
          </p>
        </div>
      </Section>
    </>
  );
}
