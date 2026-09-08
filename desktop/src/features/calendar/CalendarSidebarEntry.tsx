import { Link, useLocation } from "@tanstack/react-router";
import { CalendarDays } from "lucide-react";
import { SidebarMenuButton, SidebarMenuItem } from "@/shared/ui/sidebar";
import { SidebarMenuLabel } from "@/shared/ui/sidebar-menu-label";

export function CalendarSidebarEntry() {
  const active = useLocation({
    select: (location) => location.pathname === "/calendar",
  });
  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        asChild
        isActive={active}
        tooltip="Calendar"
        data-testid="open-calendar-view"
      >
        <Link to="/calendar">
          <CalendarDays className="h-4 w-4" />
          <SidebarMenuLabel>Calendar</SidebarMenuLabel>
        </Link>
      </SidebarMenuButton>
    </SidebarMenuItem>
  );
}
