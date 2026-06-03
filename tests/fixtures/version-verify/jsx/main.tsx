// Tiny JSX fixture — uses the classic runtime + custom factory `h` so we
// don't need react installed. tsconfig.json pins jsxFactory: "h".

function h(
  tag: string,
  props: Record<string, unknown> | null,
  ...children: unknown[]
): string {
  const label = (props && (props as { label?: string }).label) ?? "";
  return `<${tag} label="${label}">${children.join("")}</${tag}>`;
}

function Hello(props: { name: string }): string {
  return (<span label={props.name}>hi {props.name}</span>) as unknown as string;
}

const out: string = (<div label="root">{Hello({ name: "jsx" })}</div>) as unknown as string;
console.log(out);
