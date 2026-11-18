// ndg's document list carries no marker for the page being read
{
  const page = location.pathname.split("/").pop() || "index.html";

  for (const link of document.querySelectorAll('.sidebar-section[data-section="docs"] a')) {
    if (link.getAttribute("href") === page) link.setAttribute("aria-current", "page");
  }
}
