import { useState } from "react";
import { Link, NavLink, matchPath, useLocation } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { LogOut, Search } from "lucide-react";
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  Kbd,
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
import CommandPalette from "../components/CommandPalette";
import RotateLocalAdmin from "../components/RotateLocalAdmin";
import ThemeToggle from "../components/ThemeToggle";
import { useFleet } from "../lib/queries";
import { logout, useMe } from "../lib/session";
import { navSections } from "./routes";

/** `navigator.platform` is deprecated but universally supported, and this is
 *  a one-shot label choice (not feature detection) — not worth a dependency. */
function isMacPlatform(): boolean {
  return typeof navigator !== "undefined" && navigator.platform.includes("Mac");
}

/** The footer's shared full-width error banner (see `AppShell`'s comment on
 *  why it can't live inline in the ~3rem rail). Sign-out and password
 *  rotation each compose their own title/message into this one shape rather
 *  than owning separate banners. */
type AccountError = { title: string; message: React.ReactNode };

/**
 * The persistent chrome: a floating sidebar for nav, and a top bar owning the
 * collapse control and the fleet summary. Built on rnui's Sidebar suite
 * (collapsing, a mobile sheet, and the Cmd/Ctrl+B shortcut come from
 * `SidebarProvider` for free) rather than a hand-rolled `<aside>`.
 */
export default function AppShell({ children }: { children: React.ReactNode }) {
  // Lifted above the sidebar rather than kept local to AccountFooter: at rail
  // width the footer is ~3rem wide, nowhere near enough to show an error
  // message, so this renders as a full-width banner instead -- the operator
  // must be able to tell an account action failed regardless of sidebar
  // state. Shared by sign-out and password rotation, the footer's two
  // account actions.
  const [accountError, setAccountError] = useState<AccountError | null>(null);
  // Mounted once, here, rather than per-page: the palette's own fleet query
  // is `enabled` only while `paletteOpen` is true (see `useFleet`'s options),
  // so this doesn't add a background poll to pages that don't need the fleet.
  const [paletteOpen, setPaletteOpen] = useState(false);

  return (
    <SidebarProvider>
      {accountError !== null && (
        <Alert
          variant="destructive"
          className="fixed inset-x-0 top-0 z-50 rounded-none border-x-0 border-t-0"
        >
          <AlertTitle>{accountError.title}</AlertTitle>
          <AlertDescription>{accountError.message}</AlertDescription>
        </Alert>
      )}
      <CommandPalette open={paletteOpen} onOpenChange={setPaletteOpen} />
      <FleetSidebar onAccountError={setAccountError} />
      {/* min-w-0 + overflow-x-hidden are load-bearing for wide content (the
          units table runs ~130 rows with 90-char unit names): SidebarInset is a
          flex child, and a flex item defaults to `min-width: auto`, so without
          them it refuses to shrink below its content's intrinsic width, the
          document grows wider than the viewport, and the PAGE takes the
          horizontal scroll instead of the table's own scroll container. */}
      <SidebarInset className="min-w-0 overflow-x-hidden">
        <TopBar onOpenPalette={() => setPaletteOpen(true)} />
        <div className="mx-auto w-full min-w-0 max-w-6xl p-4">{children}</div>
      </SidebarInset>
    </SidebarProvider>
  );
}

/**
 * The header row: collapse control on the left; fleet summary, the palette
 * search trigger and the theme toggle on the right. The collapse control
 * lives out here rather than on the sidebar because at rail width there is
 * no good home for it, and on mobile the sidebar is an off-canvas sheet — a
 * control inside it would be unreachable.
 */
function TopBar({ onOpenPalette }: { onOpenPalette: () => void }) {
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
      {/* Search + theme controls live in the header — moved out of the sidebar
          footer, which is a sheet on mobile and a ~3rem rail when collapsed: the
          header is the one place these are always one tap away. `ml-auto` sits
          on the summary when it renders, else on this cluster, so the group is
          right-aligned in both states. */}
      <div
        className={summary === null ? "ml-auto flex items-center gap-2" : "flex items-center gap-2"}
      >
        <CommandPaletteTrigger onOpen={onOpenPalette} />
        <ThemeToggle showLabel={false} />
      </div>
    </header>
  );
}

/**
 * The Ctrl/Cmd+K trigger, in the top bar — styled as a search FIELD rather
 * than a button. It is still a real <button> (it opens the palette, it
 * doesn't accept typing), dressed in the same idiom as the app's filter
 * inputs: bordered, mono, muted placeholder text. The Kbd hint and the wide
 * field are md+ only; on phones it shrinks to an icon-sized tap target since
 * there's no hardware keyboard to honor the hint anyway.
 */
function CommandPaletteTrigger({ onOpen }: { onOpen: () => void }) {
  return (
    <button
      type="button"
      aria-label="Search"
      title="Search"
      onClick={onOpen}
      className="flex h-8 items-center gap-2 border border-input bg-transparent px-2.5 font-mono text-xs text-muted-foreground transition-colors hover:border-ring hover:text-foreground md:w-64"
    >
      <Search className="size-4 shrink-0" />
      <span className="hidden flex-1 text-left md:inline">Search…</span>
      <Kbd className="hidden md:inline-flex">{isMacPlatform() ? "⌘K" : "Ctrl K"}</Kbd>
    </button>
  );
}

/**
 * Identity + the two account actions (rotate, sign-out), one row in the
 * sidebar footer. ThemeToggle and the palette trigger moved to the TopBar.
 *
 * At rail width the identity disappears entirely rather than being clipped
 * with CSS (which would still size its parent), and the actions stack
 * vertically: two squares plus a gap don't fit the ~3rem rail.
 */
function AccountFooter({
  rail,
  onAccountError,
}: {
  rail: boolean;
  onAccountError: (error: AccountError | null) => void;
}) {
  const { data } = useMe();
  const queryClient = useQueryClient();
  const identity = data?.display_name ?? data?.email ?? data?.subject ?? null;

  const handleSignOut = () => {
    onAccountError(null);
    void logout()
      .then(async () => {
        // Invalidating an ACTIVELY OBSERVED query notifies its observer (the
        // Gate, mounted for the app's whole lifetime) directly and
        // immediately -- that's what actually flips the shell to <SignIn/>.
        // This must run, and be awaited, BEFORE `clear()`: `clear()` removes
        // the ["me"] query object from the cache entirely, and nothing then
        // forces Gate's observer to notice it's gone -- `clear()` alone leaves
        // the shell stuck showing the previous session's data through several
        // 401'd polls.
        await queryClient.invalidateQueries({ queryKey: ["me"] });
        // Now that the gate has flipped (unmounting the pages that owned
        // fleet/machine/docker/systemd queries), drop everything else too, so
        // the next sign-in on this browser starts from a clean cache instead
        // of flashing the previous operator's stale rows for up to a poll
        // interval.
        queryClient.clear();
      })
      .catch((err: unknown) => {
        // `logout()` throws when the server responded with anything but
        // 2xx. The server fails closed on a delete error (it does NOT clear
        // the cookie), so the session is still live -- do NOT invalidate or
        // clear the query cache here, or the shell flips to <SignIn/> while
        // the operator is actually still signed in.
        const message = err instanceof Error ? err.message : "Sign-out failed.";
        onAccountError({
          title: "Sign-out failed",
          message: (
            <>
              {message} Your session is still active on this server -- you are NOT signed out. Try
              again before walking away from this browser.
            </>
          ),
        });
      });
  };

  return (
    <>
      {/* One row: identity truncating on the left, the two account actions on
          the right — not stacked. `min-w-0 flex-1` is what lets the identity
          string yield space instead of pushing the buttons to a second line.
          ThemeToggle lives in the TopBar now. */}
      <div className={rail ? "flex flex-col items-center gap-1" : "flex w-full items-center gap-2"}>
        {!rail && identity !== null && (
          <div
            title={identity}
            className="min-w-0 flex-1 truncate font-mono text-[10px] uppercase tracking-widest text-muted-foreground"
          >
            {identity}
          </div>
        )}
        {/* ml-auto (not justify-between on the parent) so the actions stay
            right-aligned even when the identity is absent. */}
        <div
          className={rail ? "flex flex-col items-center gap-1" : "ml-auto flex items-center gap-1"}
        >
          <RotateLocalAdmin
            onError={(message) =>
              onAccountError(message === null ? null : { title: "Rotation failed", message })
            }
          />
          <Button
            variant="outline"
            size="sm"
            aria-label="Sign out"
            title="Sign out"
            className="size-8 justify-center p-0"
            onClick={handleSignOut}
          >
            <LogOut className="size-4" />
          </Button>
        </div>
      </div>
    </>
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
function FleetSidebar({
  onAccountError,
}: {
  onAccountError: (error: AccountError | null) => void;
}) {
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
      {/* border-b + fixed h-12 on the wordmark: the TopBar across the inset is
          exactly h-12 with its own border-b, and the two lines must meet at the
          sidebar edge — a shorter block leaves it short of the header line. */}
      <SidebarHeader className="gap-0 border-b border-border p-0">
        <Link
          to="/machines"
          title="Argus"
          className="flex h-12 items-center justify-center bg-primary font-display text-sm tracking-widest text-primary-foreground"
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
                  const isActive =
                    matchPath({ path: r.path, end: false }, location.pathname) !== null;
                  // `end: false` above (and NavLink's own default) match this
                  // entry for any descendant route (e.g. /machines/:id), so
                  // it can be "active" without being the exact current page.
                  // Only claim aria-current="page" for an exact match; use
                  // the generic "true" token otherwise (NavLink still only
                  // emits the attribute at all when its own isActive fires).
                  const isCurrentPage =
                    matchPath({ path: r.path, end: true }, location.pathname) !== null;
                  const Icon = r.nav!.icon;
                  return (
                    <SidebarMenuItem key={r.path}>
                      <SidebarMenuButton
                        size="sm"
                        isActive={isActive}
                        title={r.nav!.label}
                        render={
                          <NavLink to={r.path} aria-current={isCurrentPage ? "page" : "true"} />
                        }
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
            ? "items-center gap-1 border-t border-border px-0 py-2"
            : "gap-1 border-t border-border px-3 py-2"
        }
      >
        <AccountFooter rail={rail} onAccountError={onAccountError} />
      </SidebarFooter>

      {/* Draggable edge strip: toggling from the sidebar's own border, in
          addition to the top bar's button. */}
      <SidebarRail />
    </Sidebar>
  );
}
