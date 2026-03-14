def greet(name: str) -> str:
    message = f"hellow, {name}"
    return message


class User:
    def __init__(self, username: str) -> None:
        self.username = username

    def display_name(self) -> str:
        return self.username.title()


user = User("narendra")
result = greet(user.display_name())
print(result)
print(user)