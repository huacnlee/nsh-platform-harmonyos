export interface NativeMenuItem {
  name: string;
  label: string;
  kind: "action";
}

interface FailedMenuRetirement {
  owner: ESObject;
  close: () => Promise<void>;
  dispose: () => void;
  attempts: number;
}

const MAX_MENU_RETIRE_ATTEMPTS = 3;

export class NativeMenuCleanup {
  private pending: Set<Promise<void>> = new Set();
  // Instance-scoped retry map with bounded attempts so failed menu closes never leak across sessions.
  private failedOwners: Map<ESObject, FailedMenuRetirement> = new Map();
  private disposed: boolean = false;

  private retryFailed(): void {
    if (this.disposed) {
      return;
    }
    this.failedOwners.forEach((entry: FailedMenuRetirement, owner: ESObject): void => {
      this.failedOwners.delete(owner);
      const retryOp = (async (): Promise<void> => {
        try {
          await entry.close();
          entry.dispose();
        } catch (error) {
          entry.attempts += 1;
          if (!this.disposed && entry.attempts < MAX_MENU_RETIRE_ATTEMPTS) {
            this.failedOwners.set(owner, entry);
          } else {
            try {
              entry.dispose();
            } catch {}
          }
          console.error(`[NSH] menu retirement retry failed: ${String(error)}`);
        }
      })();
      this.pending.add(retryOp);
      retryOp.finally((): void => {
        this.pending.delete(retryOp);
      });
    });
  }
  retire(
    owner: ESObject,
    opening: Promise<void>,
    close: () => Promise<void>,
    dispose: () => void,
  ): void {
    this.retryFailed();
    const operation = (async (): Promise<void> => {
      try {
        await opening.catch((): void => {});
        await close();
        dispose();
        this.failedOwners.delete(owner);
      } catch (error) {
        if (!this.disposed) {
          this.failedOwners.set(owner, { owner, close, dispose, attempts: 1 });
        } else {
          try {
            dispose();
          } catch {}
        }
        console.error(`[NSH] menu retirement failed: ${String(error)}`);
      }
    })();
    this.pending.add(operation);
    operation.finally((): void => {
      this.pending.delete(operation);
    });
  }
  dispose(): void {
    this.disposed = true;
    this.failedOwners.forEach((entry: FailedMenuRetirement): void => {
      try {
        entry.dispose();
      } catch {}
    });
    this.failedOwners.clear();
  }
  async settled(): Promise<void> {
    this.retryFailed();
    while (this.pending.size > 0) {
      await Promise.all(Array.from(this.pending));
    }
  }
}
