import { Link, NavLink, matchPath, useLocation } from "react-router-dom";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarTrigger,
} from "@e412/rnui-react";
import ThemeToggle from "../components/ThemeToggle";
import { useFleet } from "../lib/queries";
import { navSections } from "./routes";

/**
 * The persistent chrome: brand block, primary nav, fleet summary, theme toggle.
 * Built on rnui's Sidebar suite (collapsing, a mobile sheet, and the Cmd/Ctrl+B
 * shortcut come from `SidebarProvider` for free) rather than a hand-rolled
 * `<aside>`, so the content area keeps a constrained, readable width instead of
 * stretching full-bleed.
 */
export default function AppShell({ children }: { children: React.ReactNode }) {
  const { data: rows } = useFleet();
  const location = useLocation();
  const summary =
    rows === undefined ? null : `${rows.filter((r) => r.status === "online").length}/${rows.length} ONLINE`;

  return (
    <SidebarProvider>
      <Sidebar collapsible="offcanvas" className="group-data-[side=left]:border-r-2 border-border">
        <SidebarHeader className="gap-0 p-0">
          <Link
            to="/machines"
            className="bg-primary text-primary-foreground font-display text-sm tracking-widest px-3 py-3 block"
          >
            ARGUS
          </Link>
        </SidebarHeader>
        <SidebarContent role="navigation" aria-label="Primary">
          {navSections().map(({ section, items }) => (
            <SidebarGroup key={section}>
              <SidebarGroupLabel className="h-auto px-3 pt-3 pb-1 font-normal text-[9px] uppercase tracking-[0.16em] text-muted-foreground">
                {section}
              </SidebarGroupLabel>
              <SidebarGroupContent>
                <SidebarMenu>
                  {items.map((r) => {
                    const isActive = matchPath({ path: r.path, end: false }, location.pathname) !== null;
                    // `end: false` above (and NavLink's own default) match this
                    // entry for any descendant route (e.g. /machines/:id), so
                    // it can be "active" without being the exact current page.
                    // Only claim aria-current="page" for an exact match; use
                    // the generic "true" token otherwise (NavLink still only
                    // emits the attribute at all when its own isActive fires).
                    const isCurrentPage = matchPath({ path: r.path, end: true }, location.pathname) !== null;
                    return (
                      <SidebarMenuItem key={r.path}>
                        <SidebarMenuButton
                          size="sm"
                          isActive={isActive}
                          render={<NavLink to={r.path} aria-current={isCurrentPage ? "page" : "true"} />}
                          className="rounded-none border-l-[3px] border-transparent px-3 text-xs data-active:border-primary data-active:bg-primary/15 data-active:font-semibold data-active:text-foreground"
                        >
                          {r.nav!.label}
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    );
                  })}
                </SidebarMenu>
              </SidebarGroupContent>
            </SidebarGroup>
          ))}
        </SidebarContent>
        <SidebarFooter className="flex-row items-center justify-between gap-2 border-t-2 border-border px-3 py-2 font-mono text-[10px] text-muted-foreground">
          <ThemeToggle />
          {summary !== null && <span>{summary}</span>}
        </SidebarFooter>
      </Sidebar>
      <SidebarInset className="p-4">
        <div className="p-2">
          <SidebarTrigger />
        </div>
        <div className="mx-auto w-full max-w-6xl">{children}</div>
      </SidebarInset>
    </SidebarProvider>
  );
}
