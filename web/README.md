# App Manager web UI

The web UI is live desktop mode by default. It loads state through the Tauri 2
`invoke` bridge and does not provide browser fallbacks for native commands.

For an intentional visual walkthrough, open the app with `?demo=1` or build
with `VITE_DEMO=1`. Demo mode loads `examples/demo-state.json`, shows a
prominent sample-data banner, and keeps all mutations in memory. It never
contacts a broker, opens SSH, or installs an application.

```sh
npm run dev
npm run typecheck
npm test
```

The broker token is held only in the settings form until the native
`connect_broker` command stores it in the desktop keychain. It is not written
to browser storage, the URL, or the saved `AppState` snapshot.
