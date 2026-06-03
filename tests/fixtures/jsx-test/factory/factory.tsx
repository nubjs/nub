function h(type: any, props: any, ...children: any[]) {
  return { type: typeof type === "function" ? type.name : type, props, children };
}
const Fragment = "Fragment";

const el = <div id="test">hello</div>;
const frag = <>a<span>b</span></>;
console.log("factory:" + JSON.stringify(el));
console.log("fragment:" + JSON.stringify(frag));
