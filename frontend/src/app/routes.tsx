import type { ReactElement } from "react";
import { Plus, Server, type LucideIcon } from "lucide-react";
import EnrollPage from "../pages/EnrollPage";
import FleetPage from "../pages/FleetPage";
import MachineDetailPage from "../pages/MachineDetailPage";

/**
 * The app's routes, and which of them appear in the sidebar.
 *
 * Both `App`'s <Routes> and `AppShell`'s nav render from this list, so a new
 * page becomes a route AND a nav entry in one place. Routes without `nav` are
 * drill-downs (e.g. a single machine) rather than sections.
 */
export type AppRoute = {
  path: string;
  element: ReactElement;
  nav?: {
    label: string;
    /** Sidebar group heading; entries sharing a section render under one heading. */
    section: string;
    /**
     * Shown beside the label, and ALL that shows when the sidebar collapses
     * to its icon rail — so every entry needs one, or it collapses to a
     * blank square. Required, not optional, for exactly that reason.
     */
    icon: LucideIcon;
  };
};

export const ROUTES: AppRoute[] = [
  {
    path: "/machines",
    element: <FleetPage />,
    nav: { label: "Machines", section: "Fleet", icon: Server },
  },
  {
    path: "/enroll",
    element: <EnrollPage />,
    nav: { label: "Enroll", section: "Fleet", icon: Plus },
  },
  { path: "/machines/:id", element: <MachineDetailPage /> },
];

/** Sections in declaration order, each with its nav entries. */
export function navSections(): { section: string; items: AppRoute[] }[] {
  const out: { section: string; items: AppRoute[] }[] = [];
  for (const r of ROUTES) {
    if (r.nav === undefined) continue;
    const existing = out.find((s) => s.section === r.nav!.section);
    if (existing === undefined) out.push({ section: r.nav.section, items: [r] });
    else existing.items.push(r);
  }
  return out;
}
