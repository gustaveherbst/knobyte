import { UserRepo, makeRepo, defaultRepo } from "./repo";
import type { Entity, UserAlias } from "./models";
import * as models from "./models";
import { Logger } from "@lib/logger";
import { Clock } from "./util";

export interface Notifier {
  notify(e: Entity): void;
}

interface Deps {
  repo: UserRepo;
  clock: Clock;
}

export abstract class Service {
  abstract name(): string;
  protected logger: Logger = new Logger();
  start(): void {
    this.logger.info(this.name());
    this.logger.child().child().info("deep");
  }
}

export class UserService extends Service implements Notifier {
  private readonly repo: UserRepo;
  private clock;
  constructor(private notifier: Notifier, repo?: UserRepo) {
    super();
    this.repo = repo ?? makeRepo();
    this.clock = new Clock();
  }
  name(): string {
    return "users";
  }
  notify(e: Entity): void {
    e.describe();
  }
  start(): void {
    super.start();
  }
  async rename(id: string): Promise<void> {
    const user = await this.repo.load(id);
    user.rename("a").describe();
    this.repo.find(id)?.describe();
    this.notifier.notify(user);
    this.clock.now();
    this.clock.self().self().now();
    this.start();
    this.repo.count();
    const r = makeRepo();
    r.owner().rename("x").describe();
    defaultRepo.find(id);
    UserRepo.create().find(id);
    const m = new models.User("1", "n");
    m.describe();
    const alias: UserAlias = m;
    alias.rename("b");
    const { repo } = this;
    repo.count();
    [1].forEach(() => this.repo.find(id));
    const e = m as Entity;
    e.describe();
    const n: Notifier = this;
    n.notify(m);
    this.helper({ repo: this.repo, clock: this.clock });
    (await this.repo.load(id)).describe();
  }
  private helper({ repo, clock }: Deps): void {
    repo.count();
    clock.now();
  }
}
