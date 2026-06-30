(() => {
  const selector = ".mermaid";
  let dialog;

  const ensureDialog = () => {
    if (dialog) {
      return dialog;
    }

    dialog = document.createElement("dialog");
    dialog.className = "diagram-lightbox";
    dialog.innerHTML = `
      <div class="diagram-lightbox__bar">
        <p class="diagram-lightbox__title">Diagram preview</p>
        <button class="diagram-lightbox__close" type="button">Close</button>
      </div>
      <div class="diagram-lightbox__content">
        <div class="diagram-lightbox__frame"></div>
      </div>
    `;

    dialog.querySelector(".diagram-lightbox__close")?.addEventListener("click", () => {
      dialog.close();
    });

    dialog.addEventListener("click", (event) => {
      if (event.target === dialog) {
        dialog.close();
      }
    });

    document.body.append(dialog);
    return dialog;
  };

  const openDiagram = (diagram) => {
    const lightbox = ensureDialog();
    const frame = lightbox.querySelector(".diagram-lightbox__frame");
    if (!frame) {
      return;
    }

    frame.replaceChildren();
    const renderedSvg = diagram.querySelector("svg");

    if (renderedSvg) {
      const clone = renderedSvg.cloneNode(true);
      clone.removeAttribute("width");
      clone.removeAttribute("height");
      clone.setAttribute("preserveAspectRatio", "xMidYMid meet");
      frame.append(clone);
    } else {
      const fallback = document.createElement("pre");
      fallback.textContent = diagram.textContent?.trim() ?? "";
      frame.append(fallback);
    }

    if (typeof lightbox.showModal === "function") {
      lightbox.showModal();
    } else {
      lightbox.setAttribute("open", "");
    }
  };

  const enhanceDiagram = (diagram) => {
    if (!(diagram instanceof HTMLElement) || diagram.dataset.diagramLightbox === "ready") {
      return;
    }

    diagram.dataset.diagramLightbox = "ready";
    diagram.tabIndex = 0;
    diagram.setAttribute("role", "button");
    diagram.setAttribute("aria-label", "Open diagram in larger preview");
    diagram.title = "Click to open diagram";

    diagram.addEventListener("click", () => openDiagram(diagram));
    diagram.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        openDiagram(diagram);
      }
    });
  };

  const scan = () => {
    document.querySelectorAll(selector).forEach(enhanceDiagram);
  };

  const start = () => {
    scan();
    window.setTimeout(scan, 300);
    window.setTimeout(scan, 1200);

    const observer = new MutationObserver(scan);
    observer.observe(document.body, {
      childList: true,
      subtree: true,
    });
  };

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  } else {
    start();
  }
})();
