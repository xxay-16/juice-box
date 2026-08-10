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
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.Executors;
import java.util.concurrent.ThreadFactory;
import java.util.concurrent.ThreadPoolExecutor;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class HiCMapTileManager {
    private static final int imageTileWidth = 500;
    private static final int TILE_CACHE_CAPACITY = 128;
    private static final long SLOW_TILE_RENDER_NANOS = 100_000_000L;
    private static final long SLOW_TILE_LOG_INTERVAL_NANOS = 1_000_000_000L;
    private static final AtomicLong lastSlowTileLogNanos = new AtomicLong();
    private static final AtomicInteger tileThreadCounter = new AtomicInteger();
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
    private final Set<String> pendingTileKeys = ConcurrentHashMap.newKeySet();
    private final AtomicLong cacheGeneration = new AtomicLong();
    private final ThreadPoolExecutor tileRenderExecutor;
    private final ColorScaleHandler colorScaleHandler;

    public HiCMapTileManager(ColorScaleHandler colorScaleHandler) {
        this.colorScaleHandler = colorScaleHandler;
        this.tileRenderExecutor = (ThreadPoolExecutor) Executors.newFixedThreadPool(1, TILE_RENDER_THREAD_FACTORY);
    }

    public void clearTileCache() {
        cacheGeneration.incrementAndGet();
        pendingTileKeys.clear();
        // A color-range drag can invalidate the cache dozens of times per second.
        // Discard queued work immediately so the final range does not wait behind
        // tiles rendered for obsolete slider positions. The currently running tile
        // is harmless: the generation check below prevents it from being cached.
        tileRenderExecutor.getQueue().clear();
        synchronized (tileCacheLock) {
            tileCache.clear();
        }
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

        long maxBinCountX = zd.getXGridAxis().getBinCount();
        long maxBinCountY = zd.getYGridAxis().getBinCount();
        if (maxBinCountX < 0 || maxBinCountY < 0) return null;

        int imageWidth = maxBinCountX < imageTileWidth ? (int) maxBinCountX : imageTileWidth;
        int imageHeight = maxBinCountY < imageTileWidth ? (int) maxBinCountY : imageTileWidth;
        int bx0 = tileColumn * imageTileWidth;
        int by0 = tileRow * imageTileWidth;
        scheduleTileRender(key, cacheGeneration.get(), parent, bx0, by0, imageWidth, imageHeight, tileRow, tileColumn,
                zd, controlZd, displayOption, obsNormalizationType, ctrlNormalizationType,
                hic.getExpectedValues(), hic.getExpectedControlValues());
        return null;
    }

    public boolean isTilePending(MatrixZoomData zd, MatrixZoomData controlZd, int tileRow, int tileColumn,
                                 MatrixType displayOption, NormalizationType obsNormalizationType,
                                 NormalizationType ctrlNormalizationType) {
        String key = getTileKey(zd, controlZd, tileRow, tileColumn, displayOption,
                obsNormalizationType, ctrlNormalizationType);
        return pendingTileKeys.contains(getPendingKey(cacheGeneration.get(), key));
    }

    private void scheduleTileRender(String key, long generation, JComponent parent, int bx0, int by0,
                                    int imageWidth, int imageHeight, int tileRow, int tileColumn,
                                    MatrixZoomData zd, MatrixZoomData controlZd, MatrixType displayOption,
                                    NormalizationType obsNormalizationType, NormalizationType ctrlNormalizationType,
                                    ExpectedValueFunction expectedValues, ExpectedValueFunction expectedControlValues) {
        String pendingKey = getPendingKey(generation, key);
        if (!pendingTileKeys.add(pendingKey)) {
            return;
        }
        tileRenderExecutor.execute(() -> {
            long renderStartNanos = System.nanoTime();
            try {
                BufferedImage image = renderDataWithCPU(bx0, by0, imageWidth, imageHeight, zd, controlZd, displayOption,
                        obsNormalizationType, ctrlNormalizationType, expectedValues, expectedControlValues);
                if (image != null && cacheGeneration.get() == generation) {
                    synchronized (tileCacheLock) {
                        if (cacheGeneration.get() == generation) {
                            tileCache.put(key, new GeneralTileManager.ImageTile(image, bx0, by0));
                        }
                    }
                    SwingUtilities.invokeLater(parent::repaint);
                }
                maybeLogSlowTile(System.nanoTime() - renderStartNanos, tileRow, tileColumn, displayOption);
            } catch (Exception exception) {
                System.err.println("Unable to render heatmap tile: " + exception.getMessage());
            } finally {
                pendingTileKeys.remove(pendingKey);
            }
        });
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
