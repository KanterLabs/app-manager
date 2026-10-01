export interface MachineProfile {
  id: string;
  name: string;
  host: string;
  user?: string | null;
  port: number;
  workspaceRoots: string[];
}

export interface Workspace {
  path: string;
  repository?: string | null;
  branch?: string | null;
  commit?: string | null;
  dirty: boolean;
}

export interface ConnectionReport {
  status: string;
  message: string;
  workspaces: Workspace[];
}

export interface AppSource {
  id: string;
  name: string;
  manifestUrl: string;
  artifactBaseUrl: string;
  receiptUrl: string;
  channel: string;
  adapter: string;
  extensionUuid?: string | null;
  gnomeVersions: number[];
}

export interface Artifact {
  target: string;
  name: string;
  url: string;
  sha256: string;
  size: number;
}

export interface AppRelease {
  appId: string;
  name: string;
  version: string;
  tag: string;
  commit: string;
  repository: string;
  channel: string;
  verified: boolean;
  artifacts: Artifact[];
  fetchedAt: string;
}

export interface InstalledApp {
  appId: string;
  version: string;
  adapter: string;
  extensionUuid?: string | null;
  artifactSha256: string;
  retainedArtifact: string;
  installedAt: string;
  previous?: InstalledApp | null;
}

export interface OperationReceipt {
  id: string;
  appId: string;
  action: string;
  status: string;
  version: string;
  message: string;
  startedAt: string;
  completedAt?: string | null;
}

export interface BrokerConnection {
  url: string;
  account: string;
}

export interface RepositoryMapping {
  id: string;
  name: string;
  provider: string;
  owner: string;
  repository: string;
  workflow: string;
  defaultRef: string;
  installationId?: number | null;
}

export interface BuildRecord {
  id: string;
  repositoryId: string;
  commit: string;
  reference: string;
  provider: string;
  status: string;
  runUrl?: string | null;
  createdAt: string;
}

export interface HostInfo {
  os: string;
  architecture: string;
  gnomeVersion?: number | null;
  dataDirectory: string;
  extensionsDirectory?: string;
}

export interface AppState {
  schema: number;
  machines: MachineProfile[];
  sources: AppSource[];
  releases: AppRelease[];
  installed: InstalledApp[];
  operations: OperationReceipt[];
  broker?: BrokerConnection | null;
  repositories: RepositoryMapping[];
  builds: BuildRecord[];
  extra?: Record<string, unknown>;
}

export interface Snapshot {
  state: AppState;
  host: HostInfo;
}

export type CommandMap = {
  get_snapshot: { args: undefined; result: Snapshot };
  save_machine: { args: { profile: MachineProfile }; result: AppState };
  remove_machine: { args: { id: string }; result: AppState };
  test_machine: { args: { id: string }; result: ConnectionReport };
  discover_workspaces: { args: { id: string }; result: Workspace[] };
  save_source: { args: { source: AppSource }; result: AppState };
  remove_source: { args: { id: string }; result: AppState };
  refresh_releases: { args: undefined; result: AppState };
  install_app: { args: { appId: string }; result: AppState };
  rollback_app: { args: { appId: string }; result: AppState };
  connect_broker: { args: { url: string; account: string; token: string }; result: AppState };
  disconnect_broker: { args: undefined; result: AppState };
  refresh_broker: { args: undefined; result: AppState };
  request_build: {
    args: { repositoryId: string; reference: string; commit: string };
    result: AppState;
  };
};

export const emptyState = (): AppState => ({
  schema: 1,
  machines: [],
  sources: [],
  releases: [],
  installed: [],
  operations: [],
  broker: null,
  repositories: [],
  builds: [],
  extra: {},
});

export function normaliseState(value: Partial<AppState> | null | undefined): AppState {
  const state = value ?? {};
  return {
    schema: typeof state.schema === "number" ? state.schema : 1,
    machines: Array.isArray(state.machines) ? state.machines : [],
    sources: Array.isArray(state.sources) ? state.sources : [],
    releases: Array.isArray(state.releases) ? state.releases : [],
    installed: Array.isArray(state.installed) ? state.installed : [],
    operations: Array.isArray(state.operations) ? state.operations : [],
    broker: state.broker ?? null,
    repositories: Array.isArray(state.repositories) ? state.repositories : [],
    builds: Array.isArray(state.builds) ? state.builds : [],
    extra: state.extra ?? {},
  };
}
