import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type ReactNode,
} from "react";
import { createDemoSnapshot } from "./demo";
import {
  errorMessage,
  invokeCommand,
  isDemoMode,
  isDesktopRuntime,
} from "./bridge";
import {
  normaliseState,
  type AppRelease,
  type AppSource,
  type AppState,
  type BuildRecord,
  type CommandMap,
  type ConnectionReport,
  type HostInfo,
  type InstalledApp,
  type MachineProfile,
  type RepositoryMapping,
  type Snapshot,
  type Workspace,
} from "./types";

type View = "apps" | "builds" | "repositories" | "machines" | "settings";
type RuntimeMode = "loading" | "live" | "demo" | "browser";

type ActionError = {
  title: string;
  message: string;
  nextStep: string;
};

type ModalState =
  | { kind: "machine"; id?: string }
  | { kind: "source"; id?: string }
  | { kind: "install"; appId: string }
  | { kind: "rollback"; appId: string }
  | { kind: "build"; repositoryId?: string }
  | null;

const VIEW_LABELS: Record<View, string> = {
  apps: "Apps",
  builds: "Builds",
  repositories: "Repositories",
  machines: "Machines",
  settings: "Settings",
};

const cloneState = (state: AppState): AppState =>
  JSON.parse(JSON.stringify(state)) as AppState;

const makeId = (prefix: string): string => {
  if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
    return `${prefix}-${crypto.randomUUID()}`;
  }
  return `${prefix}-${Date.now()}`;
};

const prettyStatus = (value: string): string =>
  value
    .replace(/[_-]+/g, " ")
    .replace(/\b\w/g, (letter) => letter.toUpperCase());

const shortDigest = (digest: string): string =>
  digest.length > 18 ? `${digest.slice(0, 12)}…${digest.slice(-6)}` : digest;

const formatBytes = (size: number): string => {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${Math.round(size / 1024)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
};

const formatDate = (value: string | null | undefined): string => {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(date);
};

const hostLabel = (host: HostInfo | undefined): string => {
  if (!host) return "This laptop · not detected";
  const gnome = host.gnomeVersion ? ` · GNOME ${host.gnomeVersion}` : "";
  return `${host.os} · ${host.architecture}${gnome}`;
};

const appArtifact = (release: AppRelease, host: HostInfo | undefined, source?: AppSource) => {
  if (!release.artifacts.length) return undefined;
  if (source && source.adapter !== "gnome-extension") return undefined;
  if (!host) return release.artifacts[0];
  if (source?.gnomeVersions.length && host.gnomeVersion && !source.gnomeVersions.includes(host.gnomeVersion)) {
    return undefined;
  }
  const normalizedArchitecture = host.architecture.toLowerCase();
  const isAmd64 = ["x86_64", "amd64", "x64"].includes(normalizedArchitecture);
  const isArm64 = ["aarch64", "arm64"].includes(normalizedArchitecture);
  const architectureMatches = release.artifacts.filter((artifact) => {
    const target = artifact.target.toLowerCase();
    if (source && (target === source.adapter.toLowerCase() || target.startsWith(`${source.adapter.toLowerCase()}-`))) return true;
    if (target.includes(normalizedArchitecture)) return true;
    if (isAmd64 && ["linux-amd64", "linux-x86_64", "linux-x64"].includes(target)) return true;
    if (isArm64 && ["linux-arm64", "linux-aarch64"].includes(target)) return true;
    return false;
  });
  if (!architectureMatches.length) return undefined;
  if (source && architectureMatches.some((artifact) => artifact.target.toLowerCase().startsWith(source.adapter.toLowerCase()))) {
    return architectureMatches[0];
  }
  if (!host.gnomeVersion) return architectureMatches[0];
  const gnomeTargets = architectureMatches.filter((artifact) => /gnome[-_]/i.test(artifact.target));
  if (!gnomeTargets.length) return architectureMatches[0];
  return gnomeTargets.find((artifact) => artifact.target.includes(String(host.gnomeVersion)));
};

const matchingCompatibility = (release: AppRelease, host: HostInfo | undefined, source?: AppSource): string => {
  if (!host) return "Host compatibility is checked by the desktop adapter.";
  const artifact = appArtifact(release, host, source);
  if (!artifact) return "No published artifact matches this host yet.";
  const gnome = host.gnomeVersion ? ` · GNOME ${host.gnomeVersion}` : "";
  return `${host.os} · ${host.architecture}${gnome} · ${artifact.target}`;
};

const statusTone = (status: string): "green" | "amber" | "red" | "muted" => {
  const normalized = status.toLowerCase();
  if (["verified", "success", "completed", "connected", "up to date"].some((x) => normalized.includes(x))) {
    return "green";
  }
  if (["candidate", "queued", "running", "pending", "testing"].some((x) => normalized.includes(x))) {
    return "amber";
  }
  if (["failed", "error", "offline", "disconnected", "rejected"].some((x) => normalized.includes(x))) {
    return "red";
  }
  return "muted";
};

const validUrl = (value: string): boolean => {
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:";
  } catch {
    return false;
  }
};

const validReleaseUrl = (value: string): boolean => {
  try {
    const url = new URL(value);
    if (url.protocol === "https:") return true;
    return url.protocol === "http:" && ["localhost", "127.0.0.1", "::1", "[::1]"].includes(url.hostname);
  } catch {
    return false;
  }
};

export function validCommit(value: string): boolean {
  return /^[a-f0-9]{40}$/i.test(value.trim());
}

function App() {
  const demo = isDemoMode();
  const [mode, setMode] = useState<RuntimeMode>("loading");
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [loadError, setLoadError] = useState<ActionError | null>(null);
  const [actionError, setActionError] = useState<ActionError | null>(null);
  const [view, setView] = useState<View>("apps");
  const [pending, setPending] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [modal, setModal] = useState<ModalState>(null);
  const [machineReports, setMachineReports] = useState<Record<string, ConnectionReport>>({});
  const [discovered, setDiscovered] = useState<Record<string, Workspace[]>>({});
  const noticeTimer = useRef<number | undefined>(undefined);

  const showNotice = useCallback((message: string) => {
    setNotice(message);
    if (noticeTimer.current) window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(null), 4800);
  }, []);

  const adoptSnapshot = useCallback((next: Snapshot) => {
    setSnapshot({ ...next, state: normaliseState(next.state) });
  }, []);

  const loadLiveSnapshot = useCallback(async () => {
    const next = await invokeCommand("get_snapshot", undefined);
    adoptSnapshot(next);
    return next;
  }, [adoptSnapshot]);

  const load = useCallback(async () => {
    setLoadError(null);
    setMode("loading");
    try {
      if (demo) {
        adoptSnapshot(createDemoSnapshot());
        setMode("demo");
        return;
      }
      if (!isDesktopRuntime()) {
        setMode("browser");
        setLoadError({
          title: "Open App Manager on the desktop",
          message: "The browser build does not have access to the native command bridge.",
          nextStep: "Launch the packaged desktop app, or add ?demo=1 to this URL when you want to inspect the sample interface.",
        });
        return;
      }
      await loadLiveSnapshot();
      setMode("live");
    } catch (error) {
      setMode("live");
      setLoadError({
        title: "Could not load App Manager",
        message: errorMessage(error),
        nextStep: "Check that the desktop service is running, then try loading the snapshot again.",
      });
    }
  }, [adoptSnapshot, demo, loadLiveSnapshot]);

  useEffect(() => {
    void load();
    return () => {
      if (noticeTimer.current) window.clearTimeout(noticeTimer.current);
    };
  }, [load]);

  const runStateCommand = useCallback(
    async <K extends keyof CommandMap>(
      command: K,
      args: CommandMap[K]["args"],
      demoUpdate: (state: AppState) => AppState,
      success: string,
    ): Promise<boolean> => {
      setActionError(null);
      setPending(command);
      try {
        if (mode === "demo") {
          setSnapshot((current) =>
            current ? { ...current, state: normaliseState(demoUpdate(cloneState(current.state))) } : current,
          );
        } else {
          await invokeCommand(command, args);
          // Commands return a state, but rereading here confirms the durable on-disk state.
          await loadLiveSnapshot();
        }
        showNotice(mode === "demo" ? `${success} Sample data only.` : success);
        return true;
      } catch (error) {
        setActionError({
          title: `${prettyStatus(String(command))} failed`,
          message: errorMessage(error),
          nextStep: "Review the details, correct the connection or source, and try the action again.",
        });
        return false;
      } finally {
        setPending(null);
      }
    },
    [loadLiveSnapshot, mode, showNotice],
  );

  const state = snapshot?.state;
  const host = snapshot?.host;

  const saveMachine = useCallback(
    async (profile: MachineProfile) => {
      const ok = await runStateCommand(
        "save_machine",
        { profile },
        (current) => {
          const machines = current.machines.some((machine) => machine.id === profile.id)
            ? current.machines.map((machine) => (machine.id === profile.id ? profile : machine))
            : [...current.machines, profile];
          return { ...current, machines };
        },
        "Machine profile saved.",
      );
      if (ok) setModal(null);
    },
    [runStateCommand],
  );

  const removeMachine = useCallback(
    async (id: string) => {
      const ok = await runStateCommand(
        "remove_machine",
        { id },
        (current) => ({ ...current, machines: current.machines.filter((machine) => machine.id !== id) }),
        "Machine profile removed.",
      );
      if (ok) setModal(null);
    },
    [runStateCommand],
  );

  const saveSource = useCallback(
    async (source: AppSource) => {
      const ok = await runStateCommand(
        "save_source",
        { source },
        (current) => {
          const sources = current.sources.some((item) => item.id === source.id)
            ? current.sources.map((item) => (item.id === source.id ? source : item))
            : [...current.sources, source];
          return { ...current, sources };
        },
        "Release source saved.",
      );
      if (ok) setModal(null);
    },
    [runStateCommand],
  );

  const removeSource = useCallback(
    async (id: string) => {
      const ok = await runStateCommand(
        "remove_source",
        { id },
        (current) => ({ ...current, sources: current.sources.filter((source) => source.id !== id) }),
        "Release source removed.",
      );
      if (ok) setModal(null);
    },
    [runStateCommand],
  );

  const refreshReleases = useCallback(async () => {
    await runStateCommand("refresh_releases", undefined, (current) => current, "Release catalog refreshed.");
  }, [runStateCommand]);

  const installApp = useCallback(
    async (appId: string) => {
      const release = state?.releases.find((item) => item.appId === appId);
      if (!release) return false;
      const installed = state?.installed.find((item) => item.appId === appId);
      const source = state?.sources.find((item) => item.id === appId);
      const artifact = appArtifact(release, host, source);
      const ok = await runStateCommand(
        "install_app",
        { appId },
        (current) => {
          if (!artifact) return current;
          const next: InstalledApp = {
            appId,
            version: release.version,
            adapter: source?.adapter ?? "desktop",
            extensionUuid: source?.extensionUuid,
            artifactSha256: artifact.sha256,
            retainedArtifact: artifact.name,
            installedAt: new Date().toISOString(),
            previous: installed ? { ...installed } : null,
          };
          return {
            ...current,
            installed: current.installed.some((item) => item.appId === appId)
              ? current.installed.map((item) => (item.appId === appId ? next : item))
              : [...current.installed, next],
          };
        },
        `${release.name} ${installed ? "updated" : "installed"}.`,
      );
      if (ok) setModal(null);
      return ok;
    },
    [host, runStateCommand, state],
  );

  const rollbackApp = useCallback(
    async (appId: string) => {
      const app = state?.installed.find((item) => item.appId === appId);
      const ok = await runStateCommand(
        "rollback_app",
        { appId },
        (current) => {
          const installed = current.installed.find((item) => item.appId === appId);
          if (!installed?.previous) return current;
          return {
            ...current,
            installed: current.installed.map((item) => (item.appId === appId ? installed.previous! : item)),
          };
        },
        `${app?.appId ?? "App"} rolled back.`,
      );
      if (ok) setModal(null);
      return ok;
    },
    [runStateCommand, state],
  );

  const testMachine = useCallback(
    async (id: string) => {
      setActionError(null);
      setPending(`test_machine:${id}`);
      try {
        const report =
          mode === "demo"
            ? {
                status: "connected",
                message: "SSH connection verified in the sample state.",
                workspaces: [
                  {
                    path: "/home/shane/projects/app-manager",
                    repository: "KanterLabs/app-manager",
                    branch: "main",
                    commit: "4e2f16f3a1c5002d1f1fce8f3d2a1234567890ab",
                    dirty: false,
                  },
                ],
              }
            : await invokeCommand("test_machine", { id });
        setMachineReports((current) => ({ ...current, [id]: report }));
        showNotice(mode === "demo" ? "Connection checked. Sample data only." : "Connection checked.");
      } catch (error) {
        setActionError({
          title: "Connection test failed",
          message: errorMessage(error),
          nextStep: "Confirm the host, user, SSH configuration, and an approved workspace root before retrying.",
        });
      } finally {
        setPending(null);
      }
    },
    [mode, showNotice],
  );

  const discoverWorkspaces = useCallback(
    async (id: string) => {
      setActionError(null);
      setPending(`discover_workspaces:${id}`);
      try {
        const workspaces =
          mode === "demo"
            ? [
                {
                  path: "/home/shane/projects/app-manager",
                  repository: "KanterLabs/app-manager",
                  branch: "main",
                  commit: "4e2f16f3a1c5002d1f1fce8f3d2a1234567890ab",
                  dirty: false,
                },
              ]
            : await invokeCommand("discover_workspaces", { id });
        setDiscovered((current) => ({ ...current, [id]: workspaces }));
        showNotice(mode === "demo" ? "Workspace list refreshed. Sample data only." : "Workspace list refreshed.");
      } catch (error) {
        setActionError({
          title: "Workspace discovery failed",
          message: errorMessage(error),
          nextStep: "Check the saved machine connection and make sure at least one approved root is reachable.",
        });
      } finally {
        setPending(null);
      }
    },
    [mode, showNotice],
  );

  const connectBroker = useCallback(
    async (url: string, account: string, token: string) => {
      if (!validUrl(url) || !account.trim() || !token.trim()) {
        setActionError({
          title: "Broker details need attention",
          message: "Use an HTTP(S) broker URL, an account name, and a token.",
          nextStep: "The token is held only in this form until the native keychain enrollment finishes.",
        });
        return false;
      }
      const ok = await runStateCommand(
        "connect_broker",
        { url: url.trim(), account: account.trim(), token },
        (current) => ({ ...current, broker: { url: url.trim(), account: account.trim() } }),
        "Broker connected.",
      );
      return ok;
    },
    [runStateCommand],
  );

  const disconnectBroker = useCallback(async () => {
    await runStateCommand("disconnect_broker", undefined, (current) => ({ ...current, broker: null, repositories: [], builds: [] }), "Broker disconnected.");
  }, [runStateCommand]);

  const refreshBroker = useCallback(async () => {
    await runStateCommand("refresh_broker", undefined, (current) => current, "Broker data refreshed.");
  }, [runStateCommand]);

  const requestBuild = useCallback(
    async (repositoryId: string, reference: string, commit: string) => {
      if (!validCommit(commit)) {
        setActionError({
          title: "Commit must be exact",
          message: "The broker accepts an exact 40-character hexadecimal commit SHA.",
          nextStep: "Resolve the reference on the canonical provider and paste the resulting full SHA.",
        });
        return false;
      }
      const repository = state?.repositories.find((item) => item.id === repositoryId);
      const ok = await runStateCommand(
        "request_build",
        { repositoryId, reference: reference.trim(), commit: commit.trim() },
        (current) => {
          const build: BuildRecord = {
            id: makeId("demo-build"),
            repositoryId,
            commit: commit.trim(),
            reference: reference.trim(),
            provider: repository?.provider ?? "Broker",
            status: "queued",
            runUrl: null,
            createdAt: new Date().toISOString(),
          };
          return { ...current, builds: [build, ...current.builds] };
        },
        "Build request queued.",
      );
      if (ok) setModal(null);
      return ok;
    },
    [runStateCommand, state],
  );

  const openView = useCallback((next: View) => {
    setView(next);
    setActionError(null);
  }, []);

  const commonProps = {
    state,
    host,
    mode,
    pending,
    openView,
  };

  let content: ReactNode = null;
  if (view === "apps") {
    content = (
      <AppsView
        {...commonProps}
        onRefresh={refreshReleases}
        onAddSource={() => setModal({ kind: "source" })}
        onEditSource={(id) => setModal({ kind: "source", id })}
        onRemoveSource={removeSource}
        onReviewInstall={(appId) => setModal({ kind: "install", appId })}
        onReviewRollback={(appId) => setModal({ kind: "rollback", appId })}
      />
    );
  } else if (view === "builds") {
    content = (
      <BuildsView
        {...commonProps}
        onRefresh={refreshBroker}
        onRequestBuild={() => setModal({ kind: "build" })}
        onRequestBuildFor={(repositoryId) => setModal({ kind: "build", repositoryId })}
      />
    );
  } else if (view === "repositories") {
    content = (
      <RepositoriesView
        {...commonProps}
        onRefresh={refreshBroker}
        onRequestBuild={(repositoryId) => setModal({ kind: "build", repositoryId })}
        onOpenSettings={() => openView("settings")}
      />
    );
  } else if (view === "machines") {
    content = (
      <MachinesView
        {...commonProps}
        reports={machineReports}
        discovered={discovered}
        onAdd={() => setModal({ kind: "machine" })}
        onEdit={(id) => setModal({ kind: "machine", id })}
        onRemove={removeMachine}
        onTest={testMachine}
        onDiscover={discoverWorkspaces}
      />
    );
  } else {
    content = (
      <SettingsView
        {...commonProps}
        onConnect={connectBroker}
        onDisconnect={disconnectBroker}
        onRefresh={refreshBroker}
      />
    );
  }

  if (mode === "loading") {
    return <AppFrame view={view} mode={mode} host={undefined} onView={openView}><LoadingState /></AppFrame>;
  }

  if (mode === "browser" || loadError || !snapshot || !state || !host) {
    const fallbackError = loadError ?? {
      title: "No snapshot loaded",
      message: "App Manager did not return a desktop snapshot.",
      nextStep: "Try loading the snapshot again from the desktop app.",
    };
    return (
      <AppFrame view={view} mode={mode} host={host} onView={openView}>
        <ErrorState error={fallbackError} onRetry={load} />
      </AppFrame>
    );
  }

  return (
    <AppFrame view={view} mode={mode} host={host} onView={openView}>
      {mode === "demo" && <DemoBanner />}
      {actionError && <ActionBanner error={actionError} onRetry={load} onDismiss={() => setActionError(null)} />}
      {content}
      {notice && <div className="toast" role="status" aria-live="polite">{notice}</div>}
      {modal?.kind === "machine" && (
        <MachineModal
          machine={state.machines.find((item) => item.id === modal.id)}
          busy={pending === "save_machine"}
          onClose={() => setModal(null)}
          onSave={saveMachine}
        />
      )}
      {modal?.kind === "source" && (
        <SourceModal
          source={state.sources.find((item) => item.id === modal.id)}
          busy={pending === "save_source"}
          onClose={() => setModal(null)}
          onSave={saveSource}
        />
      )}
      {modal?.kind === "install" && (
        <InstallModal
          release={state.releases.find((item) => item.appId === modal.appId)}
          installed={state.installed.find((item) => item.appId === modal.appId)}
          host={host}
          source={state.sources.find((item) => item.id === modal.appId)}
          busy={pending === "install_app"}
          onClose={() => setModal(null)}
          onConfirm={() => installApp(modal.appId)}
        />
      )}
      {modal?.kind === "rollback" && (
        <RollbackModal
          installed={state.installed.find((item) => item.appId === modal.appId)}
          busy={pending === "rollback_app"}
          onClose={() => setModal(null)}
          onConfirm={() => rollbackApp(modal.appId)}
        />
      )}
      {modal?.kind === "build" && (
        <BuildModal
          repositories={state.repositories}
          initialRepositoryId={modal.repositoryId}
          busy={pending === "request_build"}
          onClose={() => setModal(null)}
          onSubmit={requestBuild}
        />
      )}
    </AppFrame>
  );
}

type FrameProps = {
  view: View;
  mode: RuntimeMode;
  host: HostInfo | undefined;
  onView: (view: View) => void;
  children: ReactNode;
};

function AppFrame({ view, mode, host, onView, children }: FrameProps) {
  const navItems: Array<{ view: View; icon: string }> = [
    { view: "apps", icon: "▦" },
    { view: "builds", icon: "◷" },
    { view: "repositories", icon: "⑂" },
    { view: "machines", icon: "▣" },
  ];
  return (
    <div className="window-shell">
      <header className="titlebar">
        <span className="traffic-lights" aria-hidden="true"><i /><i /><i /></span>
        <span className="window-title">App Manager</span>
        <span className={`runtime-pill ${mode === "demo" ? "sample" : mode === "live" ? "live" : "muted"}`}>
          {mode === "demo" ? "SAMPLE DATA · DEMO MODE" : mode === "live" ? "LIVE DESKTOP" : "DESKTOP SETUP"}
        </span>
      </header>
      <div className="shell-layout">
        <aside className="sidebar">
          <div className="brand"><span className="brand-mark" aria-hidden="true">▧</span><span>App Manager</span></div>
          <div className="host-card">
            <strong>This laptop</strong>
            <span className="muted">{hostLabel(host)}</span>
            <small>{host ? host.dataDirectory : "Native host information appears in the desktop app."}</small>
          </div>
          <nav aria-label="Main navigation" className="main-nav">
            {navItems.map((item) => (
              <button
                className={`nav-item ${view === item.view ? "active" : ""}`}
                key={item.view}
                type="button"
                aria-current={view === item.view ? "page" : undefined}
                onClick={() => onView(item.view)}
              >
                <span className="nav-icon" aria-hidden="true">{item.icon}</span>
                {VIEW_LABELS[item.view]}
              </button>
            ))}
          </nav>
          <div className="sidebar-bottom">
            <button className={`nav-item ${view === "settings" ? "active" : ""}`} type="button" aria-current={view === "settings" ? "page" : undefined} onClick={() => onView("settings")}>
              <span className="nav-icon" aria-hidden="true">⚙</span>Settings
            </button>
            <div className="companion-status"><span className={`status-dot ${mode === "live" ? "green" : mode === "demo" ? "amber" : "red"}`} />{mode === "live" ? "Native companion connected" : mode === "demo" ? "Preview companion" : "Native companion unavailable"}<small>App Manager desktop</small></div>
          </div>
        </aside>
        <main className="main-content">{children}</main>
      </div>
    </div>
  );
}

function LoadingState() {
  return <div className="loading-state" role="status" aria-live="polite"><span className="spinner" aria-hidden="true" /><h1>Loading your workspace</h1><p>Reading the local App Manager state.</p></div>;
}

function DemoBanner() {
  return <div className="demo-banner" role="status"><span aria-hidden="true">✦</span><div><strong>Sample data · demo mode</strong><p>This preview is deliberately separate from your desktop state. Network calls, SSH connections, broker requests, and installs are disabled.</p></div><a href={window.location.pathname}>Return to live mode</a></div>;
}

function ErrorState({ error, onRetry }: { error: ActionError; onRetry: () => void }) {
  return <div className="empty-state error-state"><div className="empty-icon" aria-hidden="true">!</div><p className="eyebrow">App Manager needs attention</p><h1>{error.title}</h1><p className="subtitle">{error.message}</p><p className="next-step"><strong>Next step</strong>{error.nextStep}</p><div className="actions"><button className="button primary" type="button" onClick={onRetry}>Try again</button>{!isDesktopRuntime() && <a className="button" href={`${window.location.pathname}?demo=1`}>Open demo mode</a>}</div></div>;
}

function ActionBanner({ error, onRetry, onDismiss }: { error: ActionError; onRetry: () => void; onDismiss: () => void }) {
  return <div className="action-banner" role="alert"><div><strong>{error.title}</strong><p>{error.message}</p><small><b>Next step:</b> {error.nextStep}</small></div><div className="actions"><button type="button" className="button quiet" onClick={onRetry}>Reload state</button><button type="button" className="icon-button" aria-label="Dismiss error" onClick={onDismiss}>×</button></div></div>;
}

function PageHeading({ eyebrow = "Your delivery workspace", title, description, actions }: { eyebrow?: string; title: string; description: string; actions?: ReactNode }) {
  return <div className="page-heading"><div><p className="eyebrow">{eyebrow}</p><h1>{title}</h1><p className="subtitle">{description}</p></div>{actions && <div className="actions">{actions}</div>}</div>;
}

function Button({ children, kind = "default", type = "button", disabled = false, onClick, className = "", form }: { children: ReactNode; kind?: "default" | "primary" | "danger" | "quiet"; type?: "button" | "submit"; disabled?: boolean; onClick?: () => void; className?: string; form?: string }) {
  return <button className={`button ${kind} ${className}`} type={type} form={form} disabled={disabled} onClick={onClick}>{children}</button>;
}

function Pill({ children, tone = "muted" }: { children: ReactNode; tone?: "green" | "amber" | "red" | "muted" }) {
  return <span className={`pill ${tone}`}>{children}</span>;
}

function EmptyState({ icon, title, message, action }: { icon: string; title: string; message: string; action?: ReactNode }) {
  return <div className="empty-state"><div className="empty-icon" aria-hidden="true">{icon}</div><h2>{title}</h2><p>{message}</p>{action && <div className="actions">{action}</div>}</div>;
}

function Stat({ label, value, note }: { label: string; value: string | number; note: string }) {
  return <div className="stat-card"><small>{label}</small><strong>{value}</strong><span>{note}</span></div>;
}

type ViewProps = {
  state: AppState | undefined;
  host: HostInfo | undefined;
  mode: RuntimeMode;
  pending: string | null;
  openView: (view: View) => void;
};

function AppsView({ state, host, pending, onRefresh, onAddSource, onEditSource, onRemoveSource, onReviewInstall, onReviewRollback, openView }: ViewProps & {
  onRefresh: () => Promise<void>;
  onAddSource: () => void;
  onEditSource: (id: string) => void;
  onRemoveSource: (id: string) => Promise<void>;
  onReviewInstall: (appId: string) => void;
  onReviewRollback: (appId: string) => void;
}) {
  const releases = state?.releases ?? [];
  const installed = state?.installed ?? [];
  const sources = state?.sources ?? [];
  const ids = useMemo(() => Array.from(new Set([...releases.map((item) => item.appId), ...installed.map((item) => item.appId), ...sources.map((item) => item.id)])), [installed, releases, sources]);
  const updates = ids.filter((id) => {
    const release = releases.find((item) => item.appId === id);
    const current = installed.find((item) => item.appId === id);
    return Boolean(release && current && release.version !== current.version);
  }).length;
  return <>
    <PageHeading title="Your apps" description="A clear path from verified release to your laptop." actions={<><Button onClick={onRefresh} disabled={pending === "refresh_releases"}>↻ Refresh releases</Button><Button kind="primary" onClick={onAddSource}>＋ Add release source</Button></>} />
    <div className="stats-grid"><Stat label="Installed apps" value={installed.length} note="On this laptop" /><Stat label="Updates available" value={updates} note={releases.length ? "Verified release catalog" : "Refresh after adding a source"} /><Stat label="Saved machines" value={state?.machines.length ?? 0} note="Development workspaces" /></div>
    {ids.length === 0 ? <EmptyState icon="▦" title="Your app shelf is empty" message="Add a release source, then refresh its validated manifest. Published receipts, compatibility, and retained artifacts will appear here." action={<Button kind="primary" onClick={onAddSource}>Add your first source</Button>} /> : <>
      <div className="section-heading"><h2>Installed and available</h2><Pill>{updates ? `${updates} update${updates === 1 ? "" : "s"} available` : "Stable channel"}</Pill></div>
      <div className="app-grid">{ids.map((id) => <AppCard key={id} release={releases.find((item) => item.appId === id)} installed={installed.find((item) => item.appId === id)} source={sources.find((item) => item.id === id)} host={host} pending={pending} onReviewInstall={onReviewInstall} onReviewRollback={onReviewRollback} />)}</div>
    </>}
    <div className="content-columns lower-grid">
      <section className="panel"><div className="section-heading"><div><h2>Release sources</h2><p className="section-copy">Manifests and complete publication receipts define what can be installed.</p></div><Button onClick={onAddSource}>Add source</Button></div>{sources.length === 0 ? <p className="empty-inline">No release sources saved yet.</p> : <div className="source-list">{sources.map((source) => <div className="source-row" key={source.id}><div><strong>{source.name}</strong><span>{source.adapter} · {source.channel}</span><code>{source.manifestUrl}</code></div><div className="actions"><Button kind="quiet" onClick={() => onEditSource(source.id)}>Edit</Button><Button kind="danger" onClick={() => onRemoveSource(source.id)}>Remove</Button></div></div>)}</div>}</section>
      <section className="note-card"><span className="note-icon" aria-hidden="true">↗</span><div><strong>Build once. Install the verified release.</strong><p>CI evidence, published receipts, compatibility, and SHA-256 digests are shown before an install is allowed.</p><Button kind="quiet" onClick={() => openView("builds")}>See builds →</Button></div></section>
    </div>
    {state?.operations && state.operations.length > 0 && <section className="panel recent-activity"><div className="section-heading"><h2>Recent activity</h2><Pill>{state.operations.length} retained receipt{state.operations.length === 1 ? "" : "s"}</Pill></div><div className="activity-list">{state.operations.slice(0, 5).map((operation) => <div className="activity-row" key={operation.id}><span className={`status-dot ${statusTone(operation.status)}`} /><div><strong>{prettyStatus(operation.action)} · {operation.version || "no version"}</strong><span>{operation.message}</span></div><small>{formatDate(operation.completedAt ?? operation.startedAt)}</small></div>)}</div></section>}
  </>;
}

function AppCard({ release, installed, source, host, pending, onReviewInstall, onReviewRollback }: { release?: AppRelease; installed?: InstalledApp; source?: AppSource; host?: HostInfo; pending: string | null; onReviewInstall: (id: string) => void; onReviewRollback: (id: string) => void }) {
  const appId = release?.appId ?? installed?.appId ?? source?.id ?? "unknown";
  const name = release?.name ?? source?.name ?? appId;
  const artifact = release ? appArtifact(release, host, source) : undefined;
  const adapterSupported = !source || source.adapter === "gnome-extension";
  const hasUpdate = Boolean(release && installed && release.version !== installed.version);
  const icon = name.slice(0, 1).toUpperCase();
  return <article className="app-card"><div className="app-card-head"><span className="app-icon" aria-hidden="true">{icon}</span><div><h3>{name}</h3><small>{source?.adapter ?? installed?.adapter ?? "Release source"}</small></div><Pill tone={!adapterSupported ? "amber" : release?.verified ? "green" : "muted"}>{!adapterSupported ? "Unavailable" : release?.verified ? "Verified" : "Awaiting release"}</Pill></div><p className="app-description">{!adapterSupported ? "This adapter is shown from saved state but is not supported by the current desktop installer." : release ? `Published ${release.channel} release from ${release.repository}.` : "This source has not produced a verified release yet."}</p><div className="key-values"><div><span>Installed</span><code>{installed?.version ?? "Not installed"}</code></div><div><span>Available</span><code>{release?.version ?? "—"}</code></div><div><span>Compatibility</span><span>{release ? matchingCompatibility(release, host, source) : "Refresh a source to check"}</span></div>{artifact && <div><span>SHA-256</span><code title={artifact.sha256}>{shortDigest(artifact.sha256)}</code></div>}</div><div className="card-footer"><div className="actions">{release && <Button kind={hasUpdate || !installed ? "primary" : "default"} disabled={pending === "install_app" || !artifact || !release.verified || !adapterSupported} onClick={() => onReviewInstall(appId)}>{!adapterSupported ? "Adapter unavailable" : hasUpdate ? "Review update" : installed ? "Review install" : "Review install"}</Button>}{installed?.previous && adapterSupported && <Button kind="quiet" disabled={pending === "rollback_app"} onClick={() => onReviewRollback(appId)}>Rollback</Button>}</div>{installed && !hasUpdate && release && adapterSupported && <Pill tone="green">Up to date</Pill>}</div></article>;
}

function BuildsView({ state, pending, onRefresh, onRequestBuild, onRequestBuildFor }: ViewProps & { onRefresh: () => Promise<void>; onRequestBuild: () => void; onRequestBuildFor: (repositoryId: string) => void }) {
  const repositories = state?.repositories ?? [];
  const builds = state?.builds ?? [];
  const repoFor = (id: string) => repositories.find((repository) => repository.id === id);
  return <>
    <PageHeading title="Builds" description="Follow a committed change all the way to a verified release." actions={<><Button onClick={onRefresh} disabled={!state?.broker || pending === "refresh_broker"}>↻ Refresh status</Button><Button kind="primary" onClick={onRequestBuild} disabled={!state?.broker || repositories.length === 0}>＋ Request build</Button></>} />
    {!state?.broker ? <SetupCard title="Connect a broker to request builds" message="Build status is scoped to the account configured in your self-hosted broker." action={<Button kind="primary" onClick={onRequestBuild}>Open build request</Button>} /> : builds.length === 0 ? <EmptyState icon="◷" title="No builds yet" message="Choose an authorized repository and submit an exact 40-character commit SHA to start a registered workflow." action={repositories.length ? <Button kind="primary" onClick={onRequestBuild}>Request a build</Button> : undefined} /> : <div className="table-wrap"><table className="data-table"><caption className="visually-hidden">Build records</caption><thead><tr><th>Repository</th><th>Revision</th><th>Provider</th><th>Status</th><th>Created</th><th /></tr></thead><tbody>{builds.map((build) => <BuildRow key={build.id} build={build} repository={repoFor(build.repositoryId)} />)}</tbody></table></div>}
    <div className="content-columns lower-grid"><section className="panel"><div className="section-heading"><div><h2>Registered workflows</h2><p className="section-copy">The broker resolves references on the canonical provider before dispatch.</p></div><Pill>{repositories.length} authorized</Pill></div>{repositories.length === 0 ? <p className="empty-inline">No authorized repository mappings have been returned.</p> : <div className="workflow-list">{repositories.map((repository) => <div className="workflow-row" key={repository.id}><div><strong>{repository.owner}/{repository.repository}</strong><span>{repository.workflow} · default {repository.defaultRef}</span></div><Button onClick={() => onRequestBuildFor(repository.id)} disabled={!state?.broker}>Request</Button></div>)}</div>}</section><section className="note-card"><span className="note-icon" aria-hidden="true">◷</span><div><strong>Candidate before install</strong><p>A successful run remains a candidate until its artifact tests, published receipt, and compatibility checks pass.</p></div></section></div>
  </>;
}

function BuildRow({ build, repository }: { build: BuildRecord; repository?: RepositoryMapping }) {
  return <tr><td><strong>{repository ? `${repository.owner}/${repository.repository}` : build.repositoryId}</strong><span>{build.reference}</span></td><td><code title={build.commit}>{shortDigest(build.commit)}</code></td><td>{build.provider}</td><td><Pill tone={statusTone(build.status)}>{prettyStatus(build.status)}</Pill></td><td>{formatDate(build.createdAt)}</td><td>{build.runUrl ? <a className="row-link" href={build.runUrl} target="_blank" rel="noreferrer">View run ↗</a> : <span className="muted">Queued</span>}</td></tr>;
}

function RepositoriesView({ state, pending, onRefresh, onRequestBuild, onOpenSettings }: ViewProps & { onRefresh: () => Promise<void>; onRequestBuild: (repositoryId: string) => void; onOpenSettings: () => void }) {
  const broker = state?.broker;
  const repositories = state?.repositories ?? [];
  return <>
    <PageHeading title="Repositories" description="Connect source, build provider, and published releases." actions={<><Button onClick={onRefresh} disabled={!broker || pending === "refresh_broker"}>↻ Refresh broker</Button><Button kind="primary" onClick={onOpenSettings}>{broker ? "Broker settings" : "Connect broker"}</Button></>} />
    {!broker ? <SetupCard title="Connect your self-hosted broker" message="The broker is the authorization boundary for repositories and builds. App Manager does not store a token in browser storage." action={<Button kind="primary" onClick={onOpenSettings}>Configure broker</Button>} /> : <div className="connection-strip"><div><span className="status-dot green" /><strong>Connected account</strong><p>{broker.account} · {broker.url}</p></div><Button onClick={onOpenSettings}>Manage connection</Button></div>}
    <div className="content-columns repository-layout"><section className="panel"><div className="section-heading"><div><h2>Authorized repositories</h2><p className="section-copy">Only mappings returned for this broker account can request a build.</p></div><Pill tone={repositories.length ? "green" : "muted"}>{repositories.length} mapping{repositories.length === 1 ? "" : "s"}</Pill></div>{repositories.length === 0 ? <EmptyState icon="⑂" title="No repository mappings" message={broker ? "Refresh the broker after your account allowlist is configured." : "Connect a broker to discover authorized repository mappings."} /> : <div className="repo-list">{repositories.map((repository) => <div className="repo-card" key={repository.id}><div className="repo-card-head"><div><h3>{repository.owner}/{repository.repository}</h3><span>{repository.name}</span></div><Pill>{repository.provider}</Pill></div><div className="key-values"><div><span>Workflow</span><code>{repository.workflow}</code></div><div><span>Default ref</span><code>{repository.defaultRef}</code></div><div><span>Installation</span><span>{repository.installationId ? `#${repository.installationId}` : "Managed by broker"}</span></div></div><div className="card-footer"><Button disabled={!broker} onClick={() => onRequestBuild(repository.id)}>Request a build</Button></div></div>)}</div>}</section><aside className="side-stack"><section className="panel"><h2>Account boundary</h2><div className="status-box"><strong>{broker ? "Native credential enrolled" : "Not configured"}</strong><span>{broker ? `${broker.account} can access ${repositories.length} mapping${repositories.length === 1 ? "" : "s"}.` : "Connect through Settings to enroll a broker token."}</span></div><p className="section-copy">Credentials are handed to the native keychain adapter. The web UI keeps the token only while the form is open.</p><Button kind="quiet" onClick={onOpenSettings}>Open settings →</Button></section><section className="note-card"><div><strong>Canonical source first</strong><p>Mirror pushes do not dispatch builds. The broker resolves the requested reference and exact commit on its canonical provider.</p></div></section></aside></div>
  </>;
}

function MachinesView({ state, pending, reports, discovered, onAdd, onEdit, onRemove, onTest, onDiscover }: ViewProps & { reports: Record<string, ConnectionReport>; discovered: Record<string, Workspace[]>; onAdd: () => void; onEdit: (id: string) => void; onRemove: (id: string) => Promise<void>; onTest: (id: string) => Promise<void>; onDiscover: (id: string) => Promise<void> }) {
  const machines = state?.machines ?? [];
  return <>
    <PageHeading title="Machines" description="Keep development workspaces within reach." actions={<Button kind="primary" onClick={onAdd}>＋ Add machine</Button>} />
    {machines.length === 0 ? <EmptyState icon="▣" title="No saved machines" message="Save an SSH profile and approved workspace roots to discover committed workspaces." action={<Button kind="primary" onClick={onAdd}>Add a development machine</Button>} /> : <div className="machine-grid">{machines.map((machine) => <MachineCard key={machine.id} machine={machine} report={reports[machine.id]} workspaces={discovered[machine.id]} pending={pending} onEdit={onEdit} onRemove={onRemove} onTest={onTest} onDiscover={onDiscover} />)}</div>}
    <div className="note-card full-note"><span className="note-icon" aria-hidden="true">▣</span><div><strong>Profiles use your existing SSH setup.</strong><p>App Manager stores the host, user, port, and approved roots. It never asks the browser for a password or private key.</p></div></div>
  </>;
}

function MachineCard({ machine, report, workspaces, pending, onEdit, onRemove, onTest, onDiscover }: { machine: MachineProfile; report?: ConnectionReport; workspaces?: Workspace[]; pending: string | null; onEdit: (id: string) => void; onRemove: (id: string) => Promise<void>; onTest: (id: string) => Promise<void>; onDiscover: (id: string) => Promise<void> }) {
  const isTesting = pending === `test_machine:${machine.id}`;
  const isDiscovering = pending === `discover_workspaces:${machine.id}`;
  return <article className="panel machine-card"><div className="machine-head"><span className="machine-icon" aria-hidden="true">▣</span><div><h2>{machine.name}</h2><small>Saved SSH connection</small></div><Pill tone={report ? statusTone(report.status) : "muted"}>{report ? prettyStatus(report.status) : "Not tested"}</Pill></div><div className="key-values"><div><span>SSH host / alias</span><code>{machine.host}</code></div><div><span>User / port</span><span>{machine.user || "SSH config"} · {machine.port}</span></div><div><span>Approved roots</span><span>{machine.workspaceRoots.length ? machine.workspaceRoots.join(", ") : "None configured"}</span></div></div>{report && <div className="report-box"><strong>{report.status}</strong><span>{report.message}</span></div>}<div className="card-footer"><div className="actions"><Button onClick={() => void onTest(machine.id)} disabled={isTesting}>{isTesting ? "Testing…" : "Test connection"}</Button><Button onClick={() => void onDiscover(machine.id)} disabled={isDiscovering}>{isDiscovering ? "Discovering…" : "Discover workspaces"}</Button><Button kind="quiet" onClick={() => onEdit(machine.id)}>Edit</Button></div><Button kind="danger" onClick={() => void onRemove(machine.id)}>Remove</Button></div>{workspaces && <div className="workspace-list"><div className="section-heading"><strong>Discovered workspaces</strong><small>{workspaces.length}</small></div>{workspaces.length ? workspaces.map((workspace) => <div className="workspace-row" key={workspace.path}><div><code>{workspace.path}</code><span>{workspace.repository ?? "No repository"} · {workspace.branch ?? "detached"}</span></div><Pill tone={workspace.dirty ? "amber" : "green"}>{workspace.dirty ? "Dirty" : "Clean"}</Pill></div>) : <p className="empty-inline">No repositories were found under the approved roots.</p>}</div>}</article>;
}

function SettingsView({ state, pending, onConnect, onDisconnect, onRefresh }: ViewProps & { onConnect: (url: string, account: string, token: string) => Promise<boolean>; onDisconnect: () => Promise<void>; onRefresh: () => Promise<void> }) {
  return <><PageHeading eyebrow="Settings" title="Broker connection" description="Use a self-hosted broker for authorized repositories and build status." actions={<Button onClick={onRefresh} disabled={!state?.broker || pending === "refresh_broker"}>↻ Refresh connection</Button>} /><BrokerForm broker={state?.broker} busy={pending === "connect_broker"} onConnect={onConnect} onDisconnect={onDisconnect} /><section className="content-columns lower-grid"><section className="panel"><div className="section-heading"><div><h2>What is stored</h2><p className="section-copy">The durable state snapshot keeps the broker URL and account label only.</p></div></div><ul className="check-list"><li><span>✓</span><div><strong>Native credential storage</strong><p>The bearer token is sent to the desktop keychain adapter during enrollment.</p></div></li><li><span>✓</span><div><strong>Account scoped mappings</strong><p>Repositories and builds are refreshed for the connected broker account.</p></div></li><li><span>✓</span><div><strong>Easy revocation</strong><p>Disconnect removes the saved broker connection from App Manager state.</p></div></li></ul></section><section className="note-card"><span className="note-icon" aria-hidden="true">⌁</span><div><strong>Self-hosted by design</strong><p>Deployment authentication belongs to the broker. The desktop app only uses the account credential it receives through native storage.</p></div></section></section></>;
}

function SetupCard({ title, message, action }: { title: string; message: string; action?: ReactNode }) {
  return <section className="setup-card"><span className="setup-icon" aria-hidden="true">⌁</span><div><h2>{title}</h2><p>{message}</p>{action && <div className="actions">{action}</div>}</div></section>;
}

function Modal({ title, description, onClose, children, footer }: { title: string; description?: string; onClose: () => void; children: ReactNode; footer?: ReactNode }) {
  const dialogRef = useRef<HTMLDivElement>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialogRef.current?.focus();
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onCloseRef.current();
    };
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("keydown", handleKeyDown);
      previousFocus.current?.focus();
    };
  }, []);
  return <div className="modal-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}><div className="modal" role="dialog" aria-modal="true" aria-labelledby="modal-title" tabIndex={-1} ref={dialogRef}><header className="modal-header"><div><h2 id="modal-title">{title}</h2>{description && <p>{description}</p>}</div><button className="icon-button" type="button" aria-label="Close dialog" onClick={onClose}>×</button></header><div className="modal-body">{children}</div>{footer && <footer className="modal-footer">{footer}</footer>}</div></div>;
}

type MachineDraft = {
  id: string;
  name: string;
  host: string;
  user: string;
  port: string;
  workspaceRoots: string;
};

function MachineModal({ machine, busy, onClose, onSave }: { machine?: MachineProfile; busy: boolean; onClose: () => void; onSave: (profile: MachineProfile) => Promise<void> }) {
  const [draft, setDraft] = useState<MachineDraft>(() => ({ id: machine?.id ?? "", name: machine?.name ?? "", host: machine?.host ?? "", user: machine?.user ?? "", port: String(machine?.port ?? 22), workspaceRoots: machine?.workspaceRoots.join("\n") ?? "" }));
  const [error, setError] = useState<string | null>(null);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const name = draft.name.trim();
    const host = draft.host.trim();
    const port = Number(draft.port);
    const roots = draft.workspaceRoots.split(/\r?\n/).map((root) => root.trim()).filter(Boolean);
    if (!name || !host || /\s/.test(host) || host.startsWith("-") || !/^[a-zA-Z0-9_.:[\]%\-]+$/.test(host)) {
      setError("Enter a display name and a valid SSH host or alias.");
      return;
    }
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      setError("Use a port from 1 to 65535.");
      return;
    }
    if (roots.some((root) => root.startsWith("-"))) {
      setError("Workspace roots cannot begin with a command-line option.");
      return;
    }
    setError(null);
    await onSave({ id: draft.id || makeId("machine"), name, host, user: draft.user.trim() || null, port, workspaceRoots: roots });
  };
  return <Modal title={machine ? "Edit machine" : "Add a machine"} description="Save an SSH profile and the workspace roots App Manager may inspect." onClose={onClose} footer={<><Button onClick={onClose}>Cancel</Button><Button kind="primary" type="submit" form="machine-form" disabled={busy}>{busy ? "Saving…" : "Save machine"}</Button></>}><form id="machine-form" onSubmit={(event) => void submit(event)}><div className="form-grid two"><label className="field"><span>Display name</span><input autoFocus value={draft.name} onChange={(event) => setDraft({ ...draft, name: event.target.value })} placeholder="Development VM" maxLength={70} /></label><label className="field"><span>SSH alias or hostname</span><input value={draft.host} onChange={(event) => setDraft({ ...draft, host: event.target.value })} placeholder="dev-vm" maxLength={200} spellCheck={false} /></label></div><div className="form-grid two"><label className="field"><span>User override <small>(optional)</small></span><input value={draft.user} onChange={(event) => setDraft({ ...draft, user: event.target.value })} placeholder="Use SSH config" maxLength={70} /></label><label className="field"><span>Port</span><input type="number" min={1} max={65535} value={draft.port} onChange={(event) => setDraft({ ...draft, port: event.target.value })} /></label></div><label className="field"><span>Approved workspace roots <small>(one path per line)</small></span><textarea rows={3} value={draft.workspaceRoots} onChange={(event) => setDraft({ ...draft, workspaceRoots: event.target.value })} placeholder="~/projects&#10;/workspace" /></label><p className="form-help">Your existing SSH config or agent supplies authentication. No password or private key is collected here.</p>{error && <p className="form-error" role="alert">{error}</p>}</form></Modal>;
}

type SourceDraft = {
  id: string;
  name: string;
  manifestUrl: string;
  artifactBaseUrl: string;
  receiptUrl: string;
  channel: string;
  adapter: string;
  extensionUuid: string;
  gnomeVersions: string;
};

function SourceModal({ source, busy, onClose, onSave }: { source?: AppSource; busy: boolean; onClose: () => void; onSave: (source: AppSource) => Promise<void> }) {
  const [draft, setDraft] = useState<SourceDraft>(() => ({ id: source?.id ?? "", name: source?.name ?? "", manifestUrl: source?.manifestUrl ?? "", artifactBaseUrl: source?.artifactBaseUrl ?? "", receiptUrl: source?.receiptUrl ?? "", channel: source?.channel ?? "stable", adapter: source?.adapter ?? "gnome-extension", extensionUuid: source?.extensionUuid ?? "", gnomeVersions: source?.gnomeVersions.join(", ") ?? "" }));
  const [error, setError] = useState<string | null>(null);
  const update = (key: keyof SourceDraft, value: string) => setDraft((current) => ({ ...current, [key]: value }));
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!draft.id.trim() || !/^[a-z0-9][a-z0-9._-]*$/i.test(draft.id.trim())) {
      setError("Enter the canonical app ID used by the manifest, such as nfl-scores.");
      return;
    }
    if (!draft.name.trim() || !validReleaseUrl(draft.manifestUrl) || !validReleaseUrl(draft.artifactBaseUrl) || !validReleaseUrl(draft.receiptUrl) || !draft.adapter.trim()) {
      setError("Enter a name, valid HTTP(S) manifest, artifact, and receipt URLs, and an adapter.");
      return;
    }
    if (draft.adapter !== "gnome-extension") {
      setError("The current desktop installer supports GNOME extension sources only.");
      return;
    }
    const rawVersions = draft.gnomeVersions.split(",").map((item) => item.trim()).filter(Boolean);
    const versions = rawVersions.map((item) => Number(item)).filter((item) => Number.isInteger(item) && item > 0 && item <= 200);
    if (!draft.extensionUuid.trim() || !/^[A-Za-z0-9._@-]{1,200}$/.test(draft.extensionUuid.trim()) || (draft.extensionUuid.match(/@/g) ?? []).length !== 1) {
      setError("A GNOME extension UUID is required, such as app@example.org.");
      return;
    }
    if (!versions.length || versions.length !== rawVersions.length || new Set(versions).size !== versions.length) {
      setError("GNOME versions should be unique comma-separated numbers, such as 48, 49, 50.");
      return;
    }
    setError(null);
    await onSave({ id: draft.id.trim(), name: draft.name.trim(), manifestUrl: draft.manifestUrl.trim(), artifactBaseUrl: draft.artifactBaseUrl.trim(), receiptUrl: draft.receiptUrl.trim(), channel: draft.channel, adapter: draft.adapter.trim(), extensionUuid: draft.extensionUuid.trim() || null, gnomeVersions: versions });
  };
  return <Modal title={source ? "Edit release source" : "Add release source"} description="Point App Manager at a schema-1 manifest and its complete published receipt." onClose={onClose} footer={<><Button onClick={onClose}>Cancel</Button><Button kind="primary" type="submit" form="source-form" disabled={busy}>{busy ? "Saving…" : "Save source"}</Button></>}><form id="source-form" onSubmit={(event) => void submit(event)}><div className="form-grid two"><label className="field"><span>Canonical app ID</span><input autoFocus={!source} value={draft.id} onChange={(event) => update("id", event.target.value)} placeholder="nfl-scores" maxLength={120} spellCheck={false} readOnly={Boolean(source)} /></label><label className="field"><span>Source name</span><input autoFocus={Boolean(source)} value={draft.name} onChange={(event) => update("name", event.target.value)} placeholder="NFL Scores" maxLength={120} /></label></div><div className="form-grid two"><label className="field"><span>Manifest URL</span><input type="url" value={draft.manifestUrl} onChange={(event) => update("manifestUrl", event.target.value)} placeholder="https://releases.example/manifest.json" spellCheck={false} /><small className="field-hint">HTTPS is required by the native release verifier outside local fixtures.</small></label><label className="field"><span>Artifact base URL</span><input type="url" value={draft.artifactBaseUrl} onChange={(event) => update("artifactBaseUrl", event.target.value)} placeholder="https://releases.example/artifacts/" spellCheck={false} /></label></div><label className="field"><span>Receipt URL</span><input type="url" value={draft.receiptUrl} onChange={(event) => update("receiptUrl", event.target.value)} placeholder="https://releases.example/receipt.json" spellCheck={false} /></label><div className="form-grid two"><label className="field"><span>Adapter</span><select value={draft.adapter} onChange={(event) => update("adapter", event.target.value)}>{draft.adapter !== "gnome-extension" && <option value={draft.adapter} disabled>{draft.adapter} (unsupported)</option>}<option value="gnome-extension">GNOME extension</option></select></label><label className="field"><span>Release channel</span><select value={draft.channel} onChange={(event) => update("channel", event.target.value)}><option value="stable">Stable</option><option value="preview">Preview</option></select></label></div><div className="form-grid two"><label className="field"><span>GNOME extension UUID</span><input value={draft.extensionUuid} onChange={(event) => update("extensionUuid", event.target.value)} placeholder="app@example.org" /></label><label className="field"><span>Supported GNOME versions <small>(comma-separated)</small></span><input value={draft.gnomeVersions} onChange={(event) => update("gnomeVersions", event.target.value)} placeholder="48, 49, 50" /></label></div>{error && <p className="form-error" role="alert">{error}</p>}</form></Modal>;
}

function InstallModal({ release, installed, source, host, busy, onClose, onConfirm }: { release?: AppRelease; installed?: InstalledApp; source?: AppSource; host: HostInfo; busy: boolean; onClose: () => void; onConfirm: () => Promise<boolean> }) {
  if (!release) return <Modal title="Release unavailable" onClose={onClose}><p className="empty-inline">There is no verified release for this app yet. Refresh the source and try again.</p><div className="actions"><Button onClick={onClose}>Close</Button></div></Modal>;
  const artifact = appArtifact(release, host, source);
  const submit = async () => { if (artifact) await onConfirm(); };
  return <Modal title={installed ? `Review update · ${release.name}` : `Review install · ${release.name}`} description="Verify the target and retained artifact before the native adapter changes this laptop." onClose={onClose} footer={<><Button onClick={onClose}>Cancel</Button><Button kind="primary" onClick={() => void submit()} disabled={busy || !artifact || !release.verified}>{busy ? "Applying…" : installed ? "Approve update" : "Approve install"}</Button></>}><div className="review-grid"><div><span>Install target</span><strong>This laptop</strong><small>{hostLabel(host)}</small></div><div><span>Version</span><strong>{installed?.version ?? "Not installed"} → {release.version}</strong><small>{release.tag} · {release.channel}</small></div><div><span>Compatibility</span><strong>{artifact ? "Matching artifact" : "No matching artifact"}</strong><small>{matchingCompatibility(release, host, source)}</small></div><div><span>Digest</span><strong>{artifact ? shortDigest(artifact.sha256) : "Unavailable"}</strong><small>{artifact ? `${artifact.name} · ${formatBytes(artifact.size)}` : "The release has no artifact for this host."}</small></div></div><div className="preserve-box"><strong>Data and rollback</strong><p>The adapter preserves application data and settings. The current artifact is retained so a compatible rollback remains possible; this action does not restore an older database automatically.</p></div>{!release.verified && <p className="form-error">This release has no verified receipt and cannot be approved.</p>}</Modal>;
}

function RollbackModal({ installed, busy, onClose, onConfirm }: { installed?: InstalledApp; busy: boolean; onClose: () => void; onConfirm: () => Promise<boolean> }) {
  if (!installed) return <Modal title="Rollback unavailable" onClose={onClose}><p className="empty-inline">There is no installed app to roll back.</p><div className="actions"><Button onClick={onClose}>Close</Button></div></Modal>;
  if (!installed.previous) return <Modal title="No retained version" onClose={onClose}><p className="empty-inline">App Manager has not retained a previous installation for {installed.appId}.</p><div className="actions"><Button onClick={onClose}>Close</Button></div></Modal>;
  return <Modal title={`Review rollback · ${installed.appId}`} description="A rollback uses the retained artifact and still runs native compatibility checks." onClose={onClose} footer={<><Button onClick={onClose}>Cancel</Button><Button kind="danger" onClick={() => void onConfirm()} disabled={busy}>{busy ? "Rolling back…" : "Approve rollback"}</Button></>}><div className="review-grid"><div><span>Install target</span><strong>This laptop</strong></div><div><span>Version</span><strong>{installed.version} → {installed.previous.version}</strong></div><div><span>Retained artifact</span><strong>{installed.previous.retainedArtifact}</strong><small>{shortDigest(installed.previous.artifactSha256)}</small></div><div><span>Data handling</span><strong>Preserved</strong><small>Older app code only; data is not automatically downgraded.</small></div></div><div className="preserve-box"><strong>Before you approve</strong><p>Rollback changes the installed application version and keeps current application data in place. Check the app’s own migration guidance if the versions have incompatible data formats.</p></div></Modal>;
}

function BuildModal({ repositories, initialRepositoryId, busy, onClose, onSubmit }: { repositories: RepositoryMapping[]; initialRepositoryId?: string; busy: boolean; onClose: () => void; onSubmit: (repositoryId: string, reference: string, commit: string) => Promise<boolean> }) {
  const initial = repositories.find((repository) => repository.id === initialRepositoryId) ?? repositories[0];
  const [repositoryId, setRepositoryId] = useState(initial?.id ?? "");
  const [reference, setReference] = useState(initial?.defaultRef ?? "main");
  const [commit, setCommit] = useState("");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const repository = repositories.find((item) => item.id === repositoryId);
    if (repository && reference === "") setReference(repository.defaultRef);
  }, [reference, repositories, repositoryId]);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!repositoryId) { setError("Choose an authorized repository."); return; }
    if (!reference.trim()) { setError("Enter a branch, tag, or other provider reference."); return; }
    if (!validCommit(commit)) { setError("Enter an exact 40-character hexadecimal commit SHA."); return; }
    setError(null);
    await onSubmit(repositoryId, reference.trim(), commit.trim());
  };
  return <Modal title="Request a build" description="The broker resolves the reference and dispatches the registered workflow." onClose={onClose} footer={<><Button onClick={onClose}>Cancel</Button><Button kind="primary" type="submit" form="build-form" disabled={busy || !repositories.length}>{busy ? "Requesting…" : "Request build"}</Button></>}><form id="build-form" onSubmit={(event) => void submit(event)}>{repositories.length ? <><label className="field"><span>Authorized repository</span><select autoFocus value={repositoryId} onChange={(event) => { setRepositoryId(event.target.value); const repo = repositories.find((item) => item.id === event.target.value); if (repo) setReference(repo.defaultRef); }}>{repositories.map((repository) => <option value={repository.id} key={repository.id}>{repository.owner}/{repository.repository} · {repository.provider}</option>)}</select></label><div className="form-grid two"><label className="field"><span>Reference</span><input value={reference} onChange={(event) => setReference(event.target.value)} placeholder="main" /></label><label className="field"><span>Exact commit SHA</span><input value={commit} onChange={(event) => setCommit(event.target.value)} placeholder="40 hexadecimal characters" maxLength={40} spellCheck={false} /></label></div><div className="status-box"><strong>Registered workflow</strong><span>{repositories.find((repository) => repository.id === repositoryId)?.workflow ?? "—"}</span><small>The broker sends expected_commit and a request ID to the provider.</small></div><p className="form-help">Uncommitted workspace changes are not included. The canonical provider must resolve this exact commit.</p></> : <p className="empty-inline">No authorized repositories are available. Connect a broker and refresh its mappings first.</p>}{error && <p className="form-error" role="alert">{error}</p>}</form></Modal>;
}

function BrokerForm({ broker, busy, onConnect, onDisconnect }: { broker?: { url: string; account: string } | null; busy: boolean; onConnect: (url: string, account: string, token: string) => Promise<boolean>; onDisconnect: () => Promise<void> }) {
  const [url, setUrl] = useState(broker?.url ?? "");
  const [account, setAccount] = useState(broker?.account ?? "");
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setUrl(broker?.url ?? "");
    setAccount(broker?.account ?? "");
    setToken("");
  }, [broker]);
  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!validUrl(url) || !account.trim() || !token.trim()) {
      setError("Use an HTTP(S) URL, account name, and token.");
      return;
    }
    setError(null);
    const ok = await onConnect(url, account, token);
    // Never retain a token after enrollment, regardless of the command result.
    setToken("");
    if (ok) setError(null);
  };
  return <section className="panel broker-panel"><div className="section-heading"><div><h2>{broker ? "Connected broker" : "Connect a broker"}</h2><p className="section-copy">Enroll a token with the native credential store to load authorized repositories and builds.</p></div>{broker && <Pill tone="green">Connected</Pill>}</div><form onSubmit={(event) => void submit(event)}><div className="form-grid two"><label className="field"><span>Broker URL</span><input autoFocus={!broker} type="url" value={url} onChange={(event) => setUrl(event.target.value)} placeholder="https://broker.example.org" spellCheck={false} /></label><label className="field"><span>Account</span><input value={account} onChange={(event) => setAccount(event.target.value)} placeholder="team-or-user" autoComplete="username" /></label></div><label className="field"><span>Bearer token</span><input type="password" value={token} onChange={(event) => setToken(event.target.value)} placeholder={broker ? "Enter a replacement token" : "Paste a broker token"} autoComplete="new-password" /></label><p className="form-help">The token lives only in this form until the native <code>connect_broker</code> command stores it. It is never written to localStorage, the URL, or the AppState profile.</p>{error && <p className="form-error" role="alert">{error}</p>}<div className="actions"><Button kind="primary" type="submit" disabled={busy}>{busy ? "Enrolling…" : broker ? "Replace token" : "Connect broker"}</Button>{broker && <Button kind="danger" onClick={() => void onDisconnect()}>Disconnect</Button>}</div></form></section>;
}

export default App;
