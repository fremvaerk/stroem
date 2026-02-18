import { useMemo } from "react";
import { Link, useLocation } from "react-router";
import {
  FolderOpen,
  LayoutDashboard,
  ListChecks,
  Play,
  LogOut,
  Server,
  Users,
  Zap,
} from "lucide-react";
import { useAuth } from "@/context/auth-context";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarGroupLabel,
  SidebarSeparator,
} from "@/components/ui/sidebar";

const navItems = [
  { title: "Dashboard", href: "/", icon: LayoutDashboard },
  { title: "Workspaces", href: "/workspaces", icon: FolderOpen },
  { title: "Tasks", href: "/tasks", icon: ListChecks },
  { title: "Jobs", href: "/jobs", icon: Play },
];

function isActive(pathname: string, href: string): boolean {
  if (href === "/") return pathname === "/";
  return pathname.startsWith(href);
}

export function AppSidebar() {
  const { pathname } = useLocation();
  const { user, authRequired, logout } = useAuth();

  const adminItems = useMemo(() => {
    const items = [{ title: "Workers", href: "/workers", icon: Server }];
    if (authRequired) {
      items.push({ title: "Users", href: "/users", icon: Users });
    }
    return items;
  }, [authRequired]);

  return (
    <Sidebar>
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" asChild>
              <Link to="/">
                <div className="flex aspect-square size-8 items-center justify-center rounded-lg bg-primary text-primary-foreground">
                  <Zap className="size-4" />
                </div>
                <div className="grid flex-1 text-left text-sm leading-tight">
                  <span className="truncate font-semibold tracking-tight">
                    Strøm
                  </span>
                  <span className="truncate text-xs text-muted-foreground">
                    Workflow Platform
                  </span>
                </div>
              </Link>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {navItems.map((item) => (
                <SidebarMenuItem key={item.href}>
                  <SidebarMenuButton
                    asChild
                    isActive={isActive(pathname, item.href)}
                    tooltip={item.title}
                  >
                    <Link to={item.href}>
                      <item.icon />
                      <span>{item.title}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter>
        <SidebarGroup>
          <SidebarGroupLabel>Admin</SidebarGroupLabel>
          <SidebarGroupContent>
            <SidebarMenu>
              {adminItems.map((item) => (
                <SidebarMenuItem key={item.href}>
                  <SidebarMenuButton
                    asChild
                    isActive={isActive(pathname, item.href)}
                    tooltip={item.title}
                  >
                    <Link to={item.href}>
                      <item.icon />
                      <span>{item.title}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
        {authRequired && user && (
          <>
            <SidebarSeparator />
            <div className="px-2 py-1">
              <p className="truncate text-sm font-medium">{user.name}</p>
              <p className="truncate text-xs text-muted-foreground">
                {user.email}
              </p>
            </div>
            <SidebarMenu>
              <SidebarMenuItem>
                <SidebarMenuButton onClick={logout} tooltip="Sign out">
                  <LogOut />
                  <span>Sign out</span>
                </SidebarMenuButton>
              </SidebarMenuItem>
            </SidebarMenu>
          </>
        )}
      </SidebarFooter>
    </Sidebar>
  );
}
