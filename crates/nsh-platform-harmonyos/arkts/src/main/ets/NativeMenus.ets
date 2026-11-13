export interface NativeMenuItem {
  name: string;
  label: string;
  kind: "action";
}

interface FailedMenuRetirement {
  owner: ESObject;
  close: () => Promise<void>;
  dispose: () => void;
}

// A failed platform close retains its ComponentContent owner and retries on later settlements.
const failedOwners: Map<ESObject, FailedMenuRetirement> = new Map();
export class NativeMenuCleanup {
  private pending: Set<Promise<void>> = new Set();
  private retryFailed(): void {
    failedOwners.forEach((entry: FailedMenuRetirement, owner: ESObject): void => {
      failedOwners.delete(owner);
      const retryOp = (async (): Promise<void> => {
        try {
          await entry.close();
          entry.dispose();
        } catch (error) {
          failedOwners.set(owner, entry);
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
        failedOwners.delete(owner);
      } catch (error) {
        failedOwners.set(owner, { owner, close, dispose });
        console.error(`[NSH] menu retirement failed: ${String(error)}`);
      }
    })();
    this.pending.add(operation);
    operation.finally((): void => {
      this.pending.delete(operation);
    });
  }
  async settled(): Promise<void> {
    this.retryFailed();
    while (this.pending.size > 0) {
      await Promise.all(Array.from(this.pending));
    }
  }
}
