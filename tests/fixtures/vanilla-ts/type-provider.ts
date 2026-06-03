console.log("SIDE_EFFECT:type-provider-loaded");

export interface Shape {
  area(): number;
}

export type Color = "red" | "green" | "blue";

export class Circle implements Shape {
  constructor(public radius: number) {}
  area() { return Math.PI * this.radius ** 2; }
}
