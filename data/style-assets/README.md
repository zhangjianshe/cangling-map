# Style image assets

Place PNG or WebP pattern images in this directory, or set `STYLE_ASSET_ROOT`
to another dedicated asset directory before starting `map-server`.

GeoCSS paths are relative to this directory. For example:

```css
* {
  fill-image: url("patterns/forest.png");
  fill-image-scale: 1;
  fill-opacity: 0.85;
  stroke: #436653;
}
```

Absolute paths, parent-directory traversal and network URLs are rejected.
