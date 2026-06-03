const React = {
  createElement(type: any, props: any, ...children: any[]) {
    const typeName = typeof type === "function" ? type.name : type;
    return { type: typeName, props: { ...props, children: children.length === 1 ? children[0] : children } };
  }
};

const Heading = ({ text }: { text: string }) => <h1 className="title">{text}</h1>;
const el = <div id="root"><Heading text="Classic" /></div>;
console.log("classic:" + JSON.stringify(el));
