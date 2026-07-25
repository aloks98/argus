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
  SidebarRail,
  SidebarTrigger,
  useSidebar,
} from "@e412/rnui-react";
import ThemeToggle from "../components/ThemeToggle";
import { useFleet } from "../lib/queries";
import { navSections } from "./routes";

/**
 * The persistent chrome: a floating sidebar for nav, and a top bar owning the
 * collapse control and the fleet summary. Built on rnui's Sidebar suite
 * (collapsing, a mobile sheet, and the Cmd/Ctrl+B shortcut come from
 * `SidebarProvider` for free) rather than a hand-rolled `<aside>`.
 */
export default function AppShell({ children }: { children: React.ReactNode }) {
  return (
    <SidebarProvider>
      <FleetSidebar />
      {/* min-w-0 + overflow-x-hidden are load-bearing for wide content (the
          units table runs ~130 rows with 90-char unit names): SidebarInset is a
          flex child, and a flex item defaults to `min-width: auto`, so without
          them it refuses to shrink below its content's intrinsic width, the
          document grows wider than the viewport, and the PAGE takes the
          horizontal scroll instead of the table's own scroll container. */}
      <SidebarInset className="min-w-0 overflow-x-hidden">
        <TopBar />
        <div className="mx-auto w-full min-w-0 max-w-6xl p-4">{children}</div>
      </SidebarInset>
    </SidebarProvider>
  );
}

/**
 * The collapse control lives out here rather than on the sidebar: at rail width
 * there is no good home for it, and on mobile the sidebar is a sheet that is
 * fully off-canvas when closed — a control inside it would be unreachable. One
 * button in a fixed place beats two that appear conditionally.
 */
function TopBar() {
  const { data: rows } = useFleet();
  const summary =
    rows === undefined
      ? null
      : `${rows.filter((r) => r.status === "online").length}/${rows.length} ONLINE`;

  return (
    <header className="sticky top-0 z-30 flex h-12 shrink-0 items-center gap-3 border-b border-border bg-background px-3">
      <SidebarTrigger aria-label="Toggle sidebar" title="Toggle sidebar" />
      {summary !== null && (
        <span className="ml-auto font-mono text-[10px] uppercase tracking-widest text-muted-foreground">
          {summary}
        </span>
      )}
    </header>
  );
}

/**
 * Split out from `AppShell` so it can call `useSidebar` — the hook needs a
 * `SidebarProvider` above it, and `AppShell` is what renders one.
 *
 * Collapsed state is read in React rather than expressed as
 * `group-data-[collapsible=icon]` CSS because at rail width the difference is
 * which elements exist at all, not how they are styled: a label hidden with CSS
 * still sizes its parent.
 */
function FleetSidebar() {
  const location = useLocation();
  const { state, isMobile } = useSidebar();
  // The mobile sheet always shows full-width content, so it is never "rail".
  const rail = state === "collapsed" && !isMobile;

  return (
    // Default variant: flush to the viewport edge, which suits the squared-off
    // "asset tag" identity better than the floating panel's inset and ring.
    // `icon` means collapsing minimises to a rail you can still navigate from
    // rather than removing the nav entirely.
    //
    // The border modifier matches rnui's own (`group-data-[side=left]:border-r`)
    // so tailwind-merge can dedupe and ours wins; an unmodified `border-r`
    // would lose to the base's higher-specificity variant. Keep the modifier
    // even now that the width matches rnui's default — dropping it would
    // quietly reintroduce that trap the next time this width changes.
    <Sidebar collapsible="icon" className="group-data-[side=left]:border-r border-border">
      <SidebarHeader className="gap-0 p-0">
        <Link
          to="/machines"
          title="Argus"
          className="block bg-primary py-3 text-center font-display text-sm tracking-widest text-primary-foreground"
        >
          {/* The wordmark has no rail form, so it becomes a single-letter mark
              rather than being truncated to something unreadable. */}
          {rail ? "A" : "ARGUS"}
        </Link>
      </SidebarHeader>

      <SidebarContent role="navigation" aria-label="Primary">
        {navSections().map(({ section, items }) => (
          <SidebarGroup key={section} className={rail ? "px-0" : undefined}>
            {/* The section heading is words only — no rail form, so it goes
                rather than being squeezed or abbreviated. */}
            {!rail && (
              <SidebarGroupLabel className="h-auto px-3 pt-3 pb-1 font-normal text-[9px] uppercase tracking-[0.16em] text-muted-foreground">
                {section}
              </SidebarGroupLabel>
            )}
            <SidebarGroupContent>
              <SidebarMenu className={rail ? "items-center gap-1 py-2" : undefined}>
                {items.map((r) => {
                  const isActive = matchPath({ path: r.path, end: false }, location.pathname) !== null;
                  // `end: false` above (and NavLink's own default) match this
                  // entry for any descendant route (e.g. /machines/:id), so
                  // it can be "active" without being the exact current page.
                  // Only claim aria-current="page" for an exact match; use
                  // the generic "true" token otherwise (NavLink still only
                  // emits the attribute at all when its own isActive fires).
                  const isCurrentPage = matchPath({ path: r.path, end: true }, location.pathname) !== null;
                  const Icon = r.nav!.icon;
                  return (
                    <SidebarMenuItem key={r.path}>
                      <SidebarMenuButton
                        size="sm"
                        isActive={isActive}
                        title={r.nav!.label}
                        render={<NavLink to={r.path} aria-current={isCurrentPage ? "page" : "true"} />}
                        className={
                          rail
                            ? "size-9 justify-center rounded-none p-0 data-active:bg-primary/20 data-active:text-foreground"
                            : "rounded-none border-l-[3px] border-transparent px-3 text-xs data-active:border-primary data-active:bg-primary/15 data-active:font-semibold data-active:text-foreground"
                        }
                      >
                        {/* In the rail the icon IS the nav entry, which is why
                            `nav.icon` is required rather than optional. */}
                        <Icon className="size-4 shrink-0" />
                        {!rail && r.nav!.label}
                      </SidebarMenuButton>
                    </SidebarMenuItem>
                  );
                })}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        ))}
      </SidebarContent>

      <SidebarFooter
        className={
          rail
            ? "items-center border-t border-border px-0 py-2"
            : "flex-row items-center border-t border-border px-3 py-2"
        }
      >
        <ThemeToggle showLabel={!rail} />
      </SidebarFooter>

      {/* Draggable edge strip: toggling from the sidebar's own border, in
          addition to the top bar's button. */}
      <SidebarRail />
    </Sidebar>
  );
}
