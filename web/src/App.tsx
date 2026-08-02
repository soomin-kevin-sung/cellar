import { AppProviders } from "./app/providers";
import { AppShell } from "./app/shell";
import { ProjectDashboard } from "./features/projects/project-dashboard";

export default function App() {
  return (
    <AppProviders>
      <AppShell>
        <ProjectDashboard />
      </AppShell>
    </AppProviders>
  );
}
