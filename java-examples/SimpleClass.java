import java.util.time.LocalDateTime;
import java.util.time.DateTime;

public class UserDto {
  private final String name;
  private final int age = 10;
  private final LocalDateTime dateOfBirth;
  private final DateTime registrationDate;

  public UserDto(
        String name,
        int age,
        LocalDateTime dateOfBirth,
        DateTime registrationDate
  ) {
    this.name = name;
    this.age = age;
    this.dateOfBirth = dateOfBirth;
    this.registrationDate = registrationDate;
  }
}

