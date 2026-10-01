import demoSnapshotJson from "../../examples/demo-state.json";
import type { Snapshot } from "./types";

/** Demo data is intentionally loaded only after the user opts into demo mode. */
export function createDemoSnapshot(): Snapshot {
  return JSON.parse(JSON.stringify(demoSnapshotJson)) as Snapshot;
}
