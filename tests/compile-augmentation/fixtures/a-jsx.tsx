// The automatic runtime resolved through tsconfig's jsxImportSource, which the
// bundler must apply at build time exactly as the runtime transform does.
const el = <div id="x" className="c">hi</div>;
const frag = <>{el}</>;
console.log("ok:" + el.props.id + ":" + el.props.className + ":" + (typeof el.type) + ":" + (frag.props.children === el));
