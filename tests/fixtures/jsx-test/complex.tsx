interface ButtonProps { label: string; disabled?: boolean; onClick?: () => void }

const Button = (props: ButtonProps) => <button {...props}>{props.label}</button>;
const show = true;

const el = (
  <>
    <Button label="OK" disabled={false} />
    {show ? <span>visible</span> : <span>hidden</span>}
    <p>end</p>
  </>
);

console.log("complex:" + JSON.stringify(el));
