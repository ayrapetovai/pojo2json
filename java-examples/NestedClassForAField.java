public class NestedClassForAField {
  private NestedClassForAFieldInner field;

  public static class NestedClassForAFieldInner {
    int x;
    int y;
  }

  public NestedClassForAField(int x, int y) {
    this.field = new NestedClassForAFieldInner(x, y);
  }
}

