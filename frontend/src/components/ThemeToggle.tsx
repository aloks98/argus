import { useEffect, useState } from "react";
import { useTheme } from "next-themes";
import { Sun, Moon } from "lucide-react";
import { Button } from "@e412/rnui-react";

/**
 * `showLabel={false}` renders an icon-only square — used in the sidebar's
 * collapsed rail, where the word would overflow the ~3rem width. The
 * `aria-label` is always present, so the label is decoration either way.
 */
export default function ThemeToggle({ showLabel = true }: { showLabel?: boolean }) {
  const { resolvedTheme, setTheme } = useTheme();
  const [mounted, setMounted] = useState(false);

  // Avoid a flash of the wrong icon before the theme has resolved client-side.
  useEffect(() => setMounted(true), []);
  if (!mounted) return null;

  const isDark = resolvedTheme === "dark";

  return (
    <Button
      variant="outline"
      size="sm"
      aria-label="Toggle theme"
      title={isDark ? "Switch to light theme" : "Switch to dark theme"}
      className={showLabel ? undefined : "size-8 justify-center p-0"}
      onClick={() => setTheme(isDark ? "light" : "dark")}
    >
      {isDark ? <Sun className="size-4" /> : <Moon className="size-4" />}
      {showLabel && (isDark ? "Light" : "Dark")}
    </Button>
  );
}
