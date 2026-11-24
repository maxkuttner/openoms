import { useState } from "react";
import { NavLink, Outlet, useLocation } from "react-router-dom";
import { Activity, BriefcaseBusiness, CandlestickChart, ChevronRight, Clock, ListOrdered, Shield } from "lucide-react";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import {
  Sidebar, SidebarContent, SidebarFooter, SidebarGroup, SidebarHeader, SidebarInset, SidebarMenu, SidebarMenuButton,
  SidebarMenuItem, SidebarMenuSub, SidebarMenuSubButton, SidebarMenuSubItem, SidebarProvider, SidebarTrigger,
} from "@/components/ui/sidebar";
import { Separator } from "@/components/ui/separator";
import { authApi } from "../login/api";
import { goToLogin } from "../api/client";
import { Logo } from "../components/Logo";
import { JobsIndicator } from "../components/JobsIndicator";
import { adminTabFor, pageFor, sidebarFor, type NavGroup, type Page } from "../nav";
import { CommandMenu } from "./CommandMenu";
import { ConnectionDot } from "./ConnectionDot";
import { UserMenu } from "./UserMenu";
import { useSession } from "./session";

const PAGE_ICONS: Record<string, typeof Shield> = {
  portfolios: BriefcaseBusiness, orders: ListOrdered, jobs: Clock, events: Activity,
};
const GROUP_ICONS: Record<NavGroup["id"], typeof Shield> = { trade: CandlestickChart, admin: Shield };

/** Admin items stand for a whole Admin Panel tab; other items for their own path. */
function isActive(page: Page, pathname: string, group?: NavGroup["id"]): boolean {
  if (group === "admin") return adminTabFor(pathname)?.id === page.id;
  if (page.path === "/") return pathname === "/";
  return pathname === page.path || pathname.startsWith(page.path + "/");
}

/** A collapsible sidebar group; starts open for Trade or when it holds the current page. */
function Group({ group, pathname }: { group: NavGroup; pathname: string }) {
  const Icon = GROUP_ICONS[group.id];
  const holdsCurrent = group.pages.some((p) => isActive(p, pathname, group.id));
  return (
    <Collapsible asChild defaultOpen={group.id === "trade" || holdsCurrent} className="group/collapsible">
      <SidebarMenuItem>
        <CollapsibleTrigger asChild>
          <SidebarMenuButton className="font-medium text-sidebar-foreground" tooltip={group.title}>
            <Icon /> <span>{group.title}</span>
            <ChevronRight className="ml-auto transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
          </SidebarMenuButton>
        </CollapsibleTrigger>
        <CollapsibleContent>
          <SidebarMenuSub>
            {group.pages.map((p) => (
              <SidebarMenuSubItem key={p.path}>
                <SidebarMenuSubButton asChild isActive={isActive(p, pathname, group.id)}>
                  <NavLink to={p.path} end={p.path === "/"}>{p.title}</NavLink>
                </SidebarMenuSubButton>
              </SidebarMenuSubItem>
            ))}
          </SidebarMenuSub>
        </CollapsibleContent>
      </SidebarMenuItem>
    </Collapsible>
  );
}

/** shadcn sidebar (Trade group, main pages, Admin group for admins) + top bar + page. */
export function Shell() {
  const { admin } = useSession();
  const { pathname } = useLocation();
  const [cmdOpen, setCmdOpen] = useState(false);
  const nav = sidebarFor(admin);
  const title = pathname.startsWith("/admin") ? "Admin Panel" : (pageFor(pathname)?.title ?? "");

  const signOut = () => {
    void authApi.logout().catch(() => undefined).then(() => goToLogin());
  };

  return (
    <SidebarProvider className="h-screen overflow-hidden">
      <Sidebar collapsible="offcanvas">
        <SidebarHeader className="flex-row items-center justify-between px-3 py-3">
          <Logo />
          <button
            type="button"
            onClick={() => setCmdOpen(true)}
            className="rounded border px-1 text-[10px] text-muted-foreground hover:text-foreground"
            aria-label="Open command menu"
          >
            ⌘K
          </button>
        </SidebarHeader>
        <SidebarContent>
          <SidebarGroup>
            <SidebarMenu>
              <Group group={nav.groups[0]} pathname={pathname} />
              {nav.pages.map((p) => {
                const Icon = PAGE_ICONS[p.id];
                return (
                  <SidebarMenuItem key={p.path}>
                    <SidebarMenuButton asChild isActive={isActive(p, pathname)} tooltip={p.title}>
                      <NavLink to={p.path}>{Icon && <Icon />}<span>{p.title}</span></NavLink>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                );
              })}
              {nav.groups.slice(1).map((g) => <Group key={g.id} group={g} pathname={pathname} />)}
            </SidebarMenu>
          </SidebarGroup>
        </SidebarContent>
        <SidebarFooter className="border-t">
          <UserMenu onSignOut={signOut} />
        </SidebarFooter>
      </Sidebar>
      <SidebarInset className="min-w-0">
        <header className="flex h-[38px] shrink-0 items-center gap-2 border-b px-2">
          <SidebarTrigger aria-label="Toggle sidebar" />
          <Separator orientation="vertical" className="h-4" />
          <span className="font-semibold">{title}</span>
          <div className="ml-auto flex items-center gap-3 pr-1.5">
            {admin && <JobsIndicator />}
            <ConnectionDot />
          </div>
        </header>
        <main className="min-h-0 flex-1 overflow-auto p-3.5">
          <Outlet />
        </main>
      </SidebarInset>
      <CommandMenu open={cmdOpen} setOpen={setCmdOpen} admin={admin} onSignOut={signOut} />
    </SidebarProvider>
  );
}
