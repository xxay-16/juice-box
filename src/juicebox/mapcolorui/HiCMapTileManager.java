/*
 * The MIT License (MIT)
 *
 * Copyright (c) 2011-2021 Broad Institute, Aiden Lab, Rice University, Baylor College of Medicine
 */

package juicebox.mapcolorui;

import juicebox.HiC;
import juicebox.HiCGlobals;
import juicebox.data.ExpectedValueFunction;
import juicebox.data.MatrixZoomData;
import juicebox.gui.SuperAdapter;
import juicebox.windowui.MatrixType;
import juicebox.windowui.NormalizationType;
import org.broad.igv.util.ObjectCache;

import javax.swing.*;
import java.awt.*;
import java.awt.image.BufferedImage;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.PriorityBlockingQueue;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class HiCMapTileManager {
    private static final int imageTileWidth = 500;
    private static final int TILE_CACHE_CAPACITY = 128;
    private static final long SLOW_TILE_RENDER_NANOS = 100_000_000L;
    private static final long SLOW_TILE_LOG_INTERVAL_NANOS = 1_000_000_000L;
    private static final AtomicLong lastSlowTileLogNanos = new AtomicLong();
    private static final AtomicInteger tileThreadCounter = new AtomicInteger();
    private static final AtomicLong tileTaskSequence = new AtomicLong();
    private static final ThreadFactory TILE_RENDER_THREAD_FACTORY = new ThreadFactory() {
        @Override
        public Thread newThread(Runnable runnable) {
            Thread thread = new Thread(runnable, "juicebox-tile-render-" + tileThreadCounter.incrementAndGet());
            thread.setDaemon(true);
            return thread;
        }
    };

    private final Object tileCacheLock = new Object();
    private final ObjectCache<String, GeneralTileManager.ImageTile> tileCache = new ObjectCache<>(TILE_CACHE_CAPACITY);
    private final Map<String, TileRenderTask> pendingTileTasks = new ConcurrentHashMap<>();
    private final AtomicLong cacheGeneration = new AtomicLong();
    private final ThreadPoolExecutor tileRenderExecutor;
    private final ColorScaleHandler colorScaleHandler;

    public HiCMapTileManager(ColorScaleHandler colorScaleHandler) {
        this.colorScaleHandler = colorScaleHandler;
        // Render one tile at a time so completed visible tiles are exposed in a
        // predictable sequence instead of several finishing in the same Swing frame.
        this.tileRenderExecutor = new ThreadPoolExecutor(1, 1, 0L, TimeUnit.MILLISECONDS,
                new PriorityBlockingQueue<>(), TILE_RENDER_THREAD_FACTORY);
    }

    public void clearTileCache() {
        cacheGeneration.incrementAndGet();
        pendingTileTasks.clear();
        // A color-range drag can invalidate the cache dozens of times per second.
        // Discard queued work immediately so the final range does not wait behind
        // tiles rendered for obsolete slider positions. The currently running tile
        // is harmless: the generation check below prevents it from being cached.
        tileRenderExecutor.getQueue().clear();
        synchronized (tileCacheLock) {
            tileCache.clear();
        }
    }

    private void requestTile(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                             MatrixType displayOption, NormalizationType obsNormalizationType,
                             NormalizationType ctrlNormalizationType, HiC hic, JComponent parent,
                             boolean visibleRequest) {
        if (tileRow < 0 || tileColumn < 0) return;
        long maxBinCountX = zd.getXGridAxis().getBinCount();
        long maxBinCountY = zd.getYGridAxis().getBinCount();
        int bx0 = tileColumn * imageTileWidth;
        int by0 = tileRow * imageTileWidth;
        if (bx0 >= maxBinCountX || by0 >= maxBinCountY) return;

        String key = getTileKey(zd, controlZd, tileRow, tileColumn, displayOption,
                obsNormalizationType, ctrlNormalizationType);
        int imageWidth = (int) Math.min(imageTileWidth, maxBinCountX - bx0);
        int imageHeight = (int) Math.min(imageTileWidth, maxBinCountY - by0);
        scheduleTileRender(key, cacheGeneration.get(), parent, bx0, by0, imageWidth, imageHeight, tileRow, tileColumn,
                zd, controlZd, displayOption, obsNormalizationType, ctrlNormalizationType,
                hic.getExpectedValues(), hic.getExpectedControlValues(), visibleRequest);
    }

    public GeneralTileManager.ImageTile getImageTile(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                                                     MatrixType displayOption, NormalizationType obsNormalizationType,
                                                     NormalizationType ctrlNormalizationType, HiC hic, JComponent parent) {
        if (zd == null) return null;
        String key = getTileKey(zd, controlZd, tileRow, tileColumn, displayOption,
                obsNormalizationType, ctrlNormalizationType);
        GeneralTileManager.ImageTile tile;
        synchronized (tileCacheLock) {
            tile = tileCache.get(key);
        }
        if (tile != null) {
            return tile;
        }
        requestTile(zd, controlZd, tileRow, tileColumn, displayOption, obsNormalizationType,
                ctrlNormalizationType, hic, parent, true);
        return null;
    }

    public void prefetchImageTile(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                                  MatrixType displayOption, NormalizationType obsNormalizationType,
                                  NormalizationType ctrlNormalizationType, HiC hic, JComponent parent) {
        if (zd == null || tileRow < 0 || tileColumn < 0) return;
        String key = getTileKey(zd, controlZd, tileRow, tileColumn, displayOption,
                obsNormalizationType, ctrlNormalizationType);
        synchronized (tileCacheLock) {
            if (tileCache.get(key) != null) return;
        }

        requestTile(zd, controlZd, tileRow, tileColumn, displayOption, obsNormalizationType,
                ctrlNormalizationType, hic, parent, false);
    }

    public boolean isTilePending(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                                 MatrixType displayOption, NormalizationType obsNormalizationType,
                                 NormalizationType ctrlNormalizationType) {
        String key = getTileKey(zd, controlZd, tileRow, tileColumn, displayOption,
                obsNormalizationType, ctrlNormalizationType);
        return pendingTileTasks.containsKey(getPendingKey(cacheGeneration.get(), key));
    }

    private void scheduleTileRender(String key, long generation, JComponent parent, int bx0, int by0,
                                    int imageWidth, int imageHeight, int tileRow, int tileColumn,
                                    MatrixZoomData zd, MatrixZoomData controlZd, MatrixType displayOption,
                                    NormalizationType obsNormalizationType, NormalizationType ctrlNormalizationType,
                                    ExpectedValueFunction expectedValues, ExpectedValueFunction expectedControlValues,
                                    boolean visibleRequest) {
        String pendingKey = getPendingKey(generation, key);
        TileRenderTask task = new TileRenderTask(pendingKey, key, generation, parent, bx0, by0, imageWidth, imageHeight,
                tileRow, tileColumn, zd, controlZd, displayOption, obsNormalizationType, ctrlNormalizationType,
                expectedValues, expectedControlValues, visibleRequest);
        TileRenderTask existing = pendingTileTasks.putIfAbsent(pendingKey, task);
        if (existing != null) {
            if (visibleRequest) {
                existing.promoteToVisible();
            }
            return;
        }
        tileRenderExecutor.execute(task);
    }

    private final class TileRenderTask implements Runnable, Comparable<TileRenderTask> {
        private final String pendingKey;
        private final String key;
        private final long generation;
        private final JComponent parent;
        private final int bx0, by0, imageWidth, imageHeight, tileRow, tileColumn;
        private final MatrixZoomData zd, controlZd;
        private final MatrixType displayOption;
        private final NormalizationType obsNormalizationType, ctrlNormalizationType;
        private final ExpectedValueFunction expectedValues, expectedControlValues;
        private final long sequence = tileTaskSequence.incrementAndGet();
        private volatile boolean visibleRequest;
        private volatile int priority;

        private TileRenderTask(String pendingKey, String key, long generation, JComponent parent,
                               int bx0, int by0, int imageWidth, int imageHeight, int tileRow, int tileColumn,
                               MatrixZoomData zd, MatrixZoomData controlZd, MatrixType displayOption,
                               NormalizationType obsNormalizationType, NormalizationType ctrlNormalizationType,
                               ExpectedValueFunction expectedValues, ExpectedValueFunction expectedControlValues,
                               boolean visibleRequest) {
            this.pendingKey = pendingKey;
            this.key = key;
            this.generation = generation;
            this.parent = parent;
            this.bx0 = bx0;
            this.by0 = by0;
            this.imageWidth = imageWidth;
            this.imageHeight = imageHeight;
            this.tileRow = tileRow;
            this.tileColumn = tileColumn;
            this.zd = zd;
            this.controlZd = controlZd;
            this.displayOption = displayOption;
            this.obsNormalizationType = obsNormalizationType;
            this.ctrlNormalizationType = ctrlNormalizationType;
            this.expectedValues = expectedValues;
            this.expectedControlValues = expectedControlValues;
            this.visibleRequest = visibleRequest;
            this.priority = visibleRequest ? 0 : 1;
        }

        private void promoteToVisible() {
            visibleRequest = true;
            if (priority == 0) return;
            // Reinsert queued prefetch work so the priority queue observes the
            // promotion. If removal fails, the task is already rendering.
            if (tileRenderExecutor.getQueue().remove(this)) {
                priority = 0;
                tileRenderExecutor.execute(this);
            } else {
                priority = 0;
            }
        }

        @Override
        public int compareTo(TileRenderTask other) {
            int priorityComparison = Integer.compare(priority, other.priority);
            return priorityComparison != 0 ? priorityComparison : Long.compare(sequence, other.sequence);
        }

        @Override
        public void run() {
            long renderStartNanos = System.nanoTime();
            try {
                BufferedImage image = renderDataWithCPU(bx0, by0, imageWidth, imageHeight, zd, controlZd, displayOption,
                        obsNormalizationType, ctrlNormalizationType, expectedValues, expectedControlValues);
                if (image != null && cacheGeneration.get() == generation) {
                    GeneralTileManager.ImageTile completedTile =
                            new GeneralTileManager.ImageTile(image, bx0, by0);
                    synchronized (tileCacheLock) {
                        if (cacheGeneration.get() == generation) {
                            tileCache.put(key, completedTile);
                        }
                    }
                    if (visibleRequest) {
                        // repaint() calls that arrive close together are normally
                        // coalesced by Swing. paintImmediately on the EDT makes each
                        // completed visible tile appear before the next one finishes.
                        SwingUtilities.invokeLater(() -> parent.paintImmediately(parent.getVisibleRect()));
                    } else {
                        SwingUtilities.invokeLater(parent::repaint);
                    }
                }
                maybeLogSlowTile(System.nanoTime() - renderStartNanos, tileRow, tileColumn, displayOption);
            } catch (Exception exception) {
                System.err.println("Unable to render heatmap tile: " + exception.getMessage());
            } finally {
                pendingTileTasks.remove(pendingKey, this);
            }
        }
    }

    private static String getPendingKey(long generation, String tileKey) {
        return generation + "|" + tileKey;
    }

    private static String getTileKey(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                                     MatrixType displayOption, NormalizationType obsNormalizationType,
                                     NormalizationType ctrlNormalizationType) {
        String controlKey = controlZd == null ? "none" : controlZd.getKey();
        return zd.getTileKey(tileRow, tileColumn, displayOption) + "_" + controlKey + "_"
                + obsNormalizationType + "_" + ctrlNormalizationType;
    }

    private static void maybeLogSlowTile(long renderNanos, int tileRow, int tileColumn, MatrixType displayOption) {
        if (renderNanos < SLOW_TILE_RENDER_NANOS) return;
        long now = System.nanoTime();
        long previous = lastSlowTileLogNanos.get();
        if (now - previous < SLOW_TILE_LOG_INTERVAL_NANOS
                || !lastSlowTileLogNanos.compareAndSet(previous, now)) {
            return;
        }
        System.err.printf("Slow heatmap tile: %.1f ms, row=%d, column=%d, display=%s%n",
                renderNanos / 1_000_000.0, tileRow, tileColumn, displayOption);
    }

    private BufferedImage renderDataWithCPU(int bx0, int by0, int imageWidth, int imageHeight,
                                            MatrixZoomData zd, MatrixZoomData controlZd, MatrixType displayOption,
                                            NormalizationType obsNormalizationType, NormalizationType ctrlNormalizationType,
                                            ExpectedValueFunction expectedValues, ExpectedValueFunction expectedControlValues) {
        BufferedImage image = new BufferedImage(imageWidth, imageHeight, BufferedImage.TYPE_INT_ARGB);
        Graphics2D graphics = image.createGraphics();
        try {
            if (HiCGlobals.isDarkulaModeEnabled) {
                graphics.setColor(Color.darkGray);
                graphics.fillRect(0, 0, imageWidth, imageHeight);
            }
            HeatmapRenderer renderer = new HeatmapRenderer(graphics, colorScaleHandler);
            if (!renderer.render(bx0, by0, imageWidth, imageHeight, zd, controlZd, displayOption,
                    obsNormalizationType, ctrlNormalizationType, expectedValues, expectedControlValues, true)) {
                return null;
            }
            return image;
        } finally {
            graphics.dispose();
        }
    }

    public void updateColorSliderFromColorScale(SuperAdapter superAdapter, MatrixType displayOption, String cacheKey) {
        colorScaleHandler.updateColorSliderFromColorScale(superAdapter, displayOption, cacheKey);
    }

}
