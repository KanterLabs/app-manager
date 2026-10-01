import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import App from "./App";
import { isDemoMode, isDesktopRuntime, invokeCommand } from "./bridge";

vi.mock("./bridge", async () => {
  const actual = await vi.importActual<typeof import("./bridge")>("./bridge");
  return {
    ...actual,
    isDemoMode: vi.fn(),
    isDesktopRuntime: vi.fn(),
    invokeCommand: vi.fn(),
  };
});

const mockedDemo = vi.mocked(isDemoMode);
const mockedDesktop = vi.mocked(isDesktopRuntime);
const mockedInvoke = vi.mocked(invokeCommand);

const liveSnapshot = {
  state: {
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
  },
  host: {
    os: "linux",
    architecture: "x86_64",
    gnomeVersion: 48,
    dataDirectory: "/tmp/app-manager",
    extensionsDirectory: "/tmp/extensions",
  },
};

describe("App Manager runtime boundaries", () => {
  beforeEach(() => {
    mockedDemo.mockReset();
    mockedDesktop.mockReset();
    mockedInvoke.mockReset();
  });

  it("explains the missing native bridge instead of showing fixture data in a browser", async () => {
    mockedDemo.mockReturnValue(false);
    mockedDesktop.mockReturnValue(false);

    render(<App />);

    expect(await screen.findByRole("heading", { name: "Open App Manager on the desktop" })).toBeTruthy();
    expect(screen.queryByText("NFL Scores")).toBeNull();
    expect(mockedInvoke).not.toHaveBeenCalled();
  });

  it("loads sample data only when demo mode is explicit", async () => {
    mockedDemo.mockReturnValue(true);
    mockedDesktop.mockReturnValue(false);

    render(<App />);

    expect(await screen.findByText("Your apps")).toBeTruthy();
    expect(screen.getByText("Sample data · demo mode")).toBeTruthy();
    expect(screen.getAllByText("NFL Scores").length).toBeGreaterThan(0);
    expect(mockedInvoke).not.toHaveBeenCalled();
  });

  it("requires an exact 40-character SHA before a build request", async () => {
    mockedDemo.mockReturnValue(true);
    mockedDesktop.mockReturnValue(false);

    render(<App />);
    await screen.findByText("Your apps");
    fireEvent.click(screen.getByRole("button", { name: "Builds" }));
    fireEvent.click(screen.getByRole("button", { name: /Request build/ }));
    fireEvent.click(screen.getByRole("button", { name: "Request build" }));

    expect((await screen.findByRole("alert")).textContent).toContain("exact 40-character hexadecimal commit SHA");
    await waitFor(() => expect(mockedInvoke).not.toHaveBeenCalled());
  });

  it("reloads the durable snapshot after a real machine mutation", async () => {
    mockedDemo.mockReturnValue(false);
    mockedDesktop.mockReturnValue(true);
    mockedInvoke.mockImplementation(async () => liveSnapshot as never);

    render(<App />);
    await screen.findByText("Your app shelf is empty");
    fireEvent.click(screen.getByRole("button", { name: "Machines" }));
    fireEvent.click(screen.getByRole("button", { name: "＋ Add machine" }));
    fireEvent.change(screen.getByLabelText("Display name"), { target: { value: "Build VM" } });
    fireEvent.change(screen.getByLabelText("SSH alias or hostname"), { target: { value: "build-vm" } });
    fireEvent.change(screen.getByLabelText(/Approved workspace roots/), { target: { value: "/workspace" } });
    fireEvent.click(screen.getByRole("button", { name: "Save machine" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    const commands = mockedInvoke.mock.calls.map(([command]) => command);
    expect(commands).toContain("save_machine");
    expect(commands.filter((command) => command === "get_snapshot").length).toBeGreaterThanOrEqual(2);
  });

  it("reports a real refresh failure without replacing state with sample data", async () => {
    mockedDemo.mockReturnValue(false);
    mockedDesktop.mockReturnValue(true);
    mockedInvoke.mockImplementation(async (command) => {
      if (command === "get_snapshot") return liveSnapshot as never;
      throw new Error("broker is offline");
    });

    render(<App />);
    await screen.findByText("Your app shelf is empty");
    fireEvent.click(screen.getByRole("button", { name: /Refresh releases/ }));

    expect((await screen.findByRole("alert")).textContent).toContain("broker is offline");
    expect(screen.queryByText("NFL Scores")).toBeNull();
  });

  it("validates a release source identity before invoking the native bridge", async () => {
    mockedDemo.mockReturnValue(false);
    mockedDesktop.mockReturnValue(true);
    mockedInvoke.mockImplementation(async () => liveSnapshot as never);

    render(<App />);
    await screen.findByText("Your app shelf is empty");
    fireEvent.click(screen.getByRole("button", { name: /Add release source/ }));
    fireEvent.click(screen.getByRole("button", { name: "Save source" }));

    expect((await screen.findByRole("alert")).textContent).toContain("canonical app ID");
    expect(mockedInvoke.mock.calls.map(([command]) => command)).not.toContain("save_source");
  });
});
