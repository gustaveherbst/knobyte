import { UserRepo } from "./repo";

class A {
  run(): number {
    return 1;
  }
}

class B {
  run(): number {
    return 2;
  }
}

export function neg(flag: boolean, list: UserRepo[], x: A | B, anyValue: any) {
  const v = flag ? new A() : new B();
  v.run();
  x.run();
  for (const r of list) r.count();
  anyValue.run();
  let shadow: A = new A();
  {
    const shadow = new B();
    shadow.run();
  }
  function inner(shadow: B) {
    shadow.run();
  }
  inner(new B());
  shadow.run();
  try {
    shadow.run();
  } catch (shadow) {
    (shadow as B).run();
  }
}

export class Holder<T extends A> {
  constructor(private item: T) {}
  use(): number {
    return this.item.run();
  }
}

class Response {
  run(): number {
    return 3;
  }
}

export async function external(res: globalThis.Response): Promise<void> {
  await res.json();
  new Response().run();
}
